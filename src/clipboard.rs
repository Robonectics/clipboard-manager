use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    io::Read,
    sync::{
        Arc,
        atomic::{self},
    },
};

use cosmic::iced::{futures::SinkExt, stream::channel};
use fslock::LockFile;
use futures::Stream;
use itertools::Itertools;
use tokio::sync::mpsc;

use crate::{app::APPID, clipboard_watcher, config::PRIVATE_MODE, db::MimeDataMap};

fn hash_data(data: &MimeDataMap) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut sorted: Vec<_> = data.iter().collect();
    sorted.sort_by(|(a, _), (b, _)| a.cmp(b));
    for (mime, content) in sorted {
        mime.hash(&mut hasher);
        content.hash(&mut hasher);
    }
    hasher.finish()
}

const CLIPBOARD_LOCK_FILE: &str = constcat::concat!("/tmp/", APPID, "-clipboard", ".lock");

#[derive(Debug, Clone)]
pub enum ClipboardMessage {
    Connected,
    Data(MimeDataMap),
    /// Means that the source was closed, or the compurer just started
    /// This means the clipboard manager must become the source, by providing the last entry
    EmptyKeyboard,
    Error(ClipboardError),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ClipboardError {
    #[error(transparent)]
    Watch(Arc<clipboard_watcher::Error>),
}

enum WatchRes {
    Data(MimeDataMap),
    Empty,
    Err(clipboard_watcher::Error),
}

pub fn sub() -> impl Stream<Item = ClipboardMessage> {
    channel(500, move |mut output| {
        async move {
            // Only one instance should watch the clipboard to prevent
            // multiple instances from interfering with each other in a loop.
            // Each instance's copy_iced writes trigger Wayland selection events
            // that all other instances see, causing cascading writes.
            let _lock = match LockFile::open(CLIPBOARD_LOCK_FILE) {
                Ok(mut lock) => {
                    if lock.try_lock().is_err() || !lock.owns_lock() {
                        info!("another instance is watching the clipboard, skipping");
                        std::future::pending::<()>().await;
                        return;
                    }
                    lock
                }
                Err(e) => {
                    warn!("failed to open clipboard lock file: {e}, skipping clipboard watcher");
                    std::future::pending::<()>().await;
                    return;
                }
            };

            match clipboard_watcher::Watcher::init() {
                Ok(mut clipboard_watcher) => {
                    let (tx, mut rx) = mpsc::channel(5);

                    tokio::task::spawn_blocking(move || {
                        loop {
                            debug!("start watching");
                            match clipboard_watcher
                                .start_watching(clipboard_watcher::Seat::Unspecified)
                            {
                                Ok(res) => {
                                    if !PRIVATE_MODE.load(atomic::Ordering::Relaxed) {
                                        // Read pipe data here in the blocking context
                                        // instead of in the async task. read_to_end is
                                        // blocking I/O and must not run on the async
                                        // runtime — a slow source app would freeze the
                                        // entire UI event loop.
                                        let mut data = MimeDataMap::new();
                                        for (mime_type, mut pipe) in res {
                                            let mut contents = Vec::new();
                                            match pipe.read_to_end(&mut contents) {
                                                Ok(len) => {
                                                    if len > 0 {
                                                        data.insert(mime_type, contents);
                                                    } else {
                                                        debug!("data is empty: {mime_type}");
                                                    }
                                                }
                                                Err(e) => {
                                                    warn!("read error on pipe: {mime_type} {e}");
                                                }
                                            }
                                        }
                                        if tx.blocking_send(WatchRes::Data(data)).is_err() {
                                            break;
                                        }
                                    } else {
                                        info!("private mode")
                                    }
                                }
                                Err(e) => match e {
                                    clipboard_watcher::Error::ClipboardEmpty => {
                                        if tx.blocking_send(WatchRes::Empty).is_err() {
                                            break;
                                        }
                                    }
                                    _ => {
                                        let _ = tx.blocking_send(WatchRes::Err(e));
                                        break;
                                    }
                                },
                            }
                        }
                    });
                    output.send(ClipboardMessage::Connected).await.unwrap();

                    let mut i = 0;
                    let mut last_hash: Option<u64> = None;
                    // Tracks whether we're waiting for new external data after
                    // an EmptyKeyboard triggered copy_iced. Suppresses the
                    // empty→write→echo→empty feedback loop.
                    let mut sent_empty = false;
                    loop {
                        let s = debug_span!("", i);
                        let _s = s.enter();
                        i += 1;

                        match rx.recv().await {
                            Some(WatchRes::Data(data)) => {
                                if !data.is_empty() {
                                    let hash = hash_data(&data);

                                    // Skip self-echoes: when copy_iced writes to clipboard,
                                    // the watcher sees it back as new data. Detect this by
                                    // comparing hashes and suppress the duplicate.
                                    if last_hash == Some(hash) {
                                        debug!("skipping self-echo clipboard event");
                                        continue;
                                    }
                                    last_hash = Some(hash);
                                    sent_empty = false;

                                    let mimes = data
                                        .iter()
                                        .map(|(m, d)| (m.to_string(), d.len()))
                                        .collect_vec();

                                    debug!("send mime types to db: {mimes:?}");
                                    output.send(ClipboardMessage::Data(data)).await.unwrap();
                                }
                            }

                            Some(WatchRes::Empty) => {
                                if sent_empty {
                                    debug!("skipping repeated empty keyboard event");
                                    continue;
                                }
                                debug!("empty keyboard");
                                sent_empty = true;
                                output.send(ClipboardMessage::EmptyKeyboard).await.unwrap();
                            }
                            Some(WatchRes::Err(e)) => {
                                output
                                    .send(ClipboardMessage::Error(ClipboardError::Watch(e.into())))
                                    .await
                                    .unwrap();
                                std::future::pending::<()>().await;
                            }
                            None => {
                                std::future::pending::<()>().await;
                            }
                        }
                    }
                }

                Err(e) => {
                    // todo: how to cancel properly?
                    // https://github.com/pop-os/cosmic-files/blob/d96d48995d49e17f01903ca4d89839eb4a1b1104/src/app.rs#L1704
                    output
                        .send(ClipboardMessage::Error(ClipboardError::Watch(e.into())))
                        .await
                        .unwrap();

                    std::future::pending::<()>().await;
                }
            };
        }
    })
}

// unfold experiment, doesn't work with channel, but better error management
/*

enum State {
    Init,
    Idle(paste_watch::Watcher),
    Error,
}

pub fn sub2() -> Subscription<Message> {
    struct Connect;

    subscription::unfold(
        std::any::TypeId::of::<Connect>(),
        State::Init,
        |state| {

            async move {
                match state {
                    State::Init => {
                        match paste_watch::Watcher::init(paste_watch::ClipboardType::Regular) {
                            Ok(watcher) => {
                                return (Message::Connected, State::Idle(watcher));
                            }
                            Err(e) => {
                                return (Message::Error(e), State::Error);
                            }
                        }
                    }
                    State::Idle(watcher) => {

                        let e = watcher.start_watching2(
                            paste_watch::Seat::Unspecified,
                            paste_watch::MimeType::Any,
                        );


                        todo!()
                    }

                    State::Error => {
                        // todo
                        todo!()
                    }
                }
            }
        },
    )
}

 */
