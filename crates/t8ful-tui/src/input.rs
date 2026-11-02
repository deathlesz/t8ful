//! Terminal event input for the run loop.
//!
//! Crossterm's reader blocks, so it lives on its own thread and forwards events
//! over a tokio channel. That lets the run loop wait on input, agent channels,
//! and one frame deadline together instead of polling the terminal every 16 ms.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event};
use tokio::sync::mpsc;

/// How long the reader blocks before checking whether it was asked to stop.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long teardown waits for the reader thread to notice the stop flag.
const JOIN_TIMEOUT: Duration = Duration::from_millis(200);

/// A single owner of the terminal event reader, with an awaitable receiver.
pub(crate) struct EventReader {
    rx: mpsc::UnboundedReceiver<Event>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EventReader {
    /// Start the reader thread. Only one may exist per process: crossterm reads
    /// from stdin, so a second reader would steal events from the first.
    pub(crate) fn spawn() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("t8ful-input".to_owned())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match event::poll(POLL_INTERVAL) {
                        Ok(true) => match event::read() {
                            Ok(event) => {
                                if tx.send(event).is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        },
                        Ok(false) => {}
                        Err(_) => break,
                    }
                }
            })
            .ok();

        Self { rx, stop, handle }
    }

    /// A reader backed by an explicit channel, for tests that drive the loop
    /// without a terminal.
    #[cfg(test)]
    pub(crate) fn from_channel(rx: mpsc::UnboundedReceiver<Event>) -> Self {
        Self {
            rx,
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    /// Wait for the next event. Returns `None` once the reader has stopped.
    pub(crate) async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }

    /// Take an already queued event without waiting.
    pub(crate) fn try_recv(&mut self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    /// Ask the reader to stop and wait for it, detaching if it does not.
    ///
    /// The thread can be inside one blocking poll, so the join is bounded: a
    /// straggler sees the flag on its next wake-up and exits on its own.
    pub(crate) fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        let Some(handle) = self.handle.take() else {
            return;
        };
        let started = Instant::now();

        while !handle.is_finished() {
            if started.elapsed() >= JOIN_TIMEOUT {
                return;
            }

            std::thread::sleep(Duration::from_millis(5));
        }

        let _ = handle.join();
    }
}

impl Drop for EventReader {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    use super::EventReader;

    #[tokio::test]
    async fn queued_events_are_delivered_and_exhausted() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut reader = EventReader::from_channel(rx);
        let event = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));

        tx.send(event.clone()).unwrap();

        assert_eq!(reader.try_recv(), Some(event.clone()));
        assert_eq!(reader.try_recv(), None);

        drop(tx);

        assert_eq!(reader.recv().await, None, "a dropped sender ends input");
    }

    #[tokio::test]
    async fn shutdown_is_prompt_without_a_thread() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut reader = EventReader::from_channel(rx);
        let started = std::time::Instant::now();

        reader.shutdown();

        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }
}
