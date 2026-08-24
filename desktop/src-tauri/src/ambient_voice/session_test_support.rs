//! Helpers shared by the two `session` test files.
//!
//! They sit here rather than in either of them so the worker seams and the
//! mute-fence regressions build on the same recorder and the same scripted
//! transcriber: a second copy of a helper is a second thing to keep true.

use super::*;

/// A shared, thread-safe list of the statuses a notifier was handed.
pub(super) type Announced = Arc<Mutex<Vec<AmbientStatus>>>;

/// Build a notifier that appends every announced status to `announced`.
pub(super) fn recorder(announced: &Announced) -> AmbientStatusNotifier {
    let announced = Arc::clone(announced);
    Arc::new(move |next: &AmbientStatus| {
        announced
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(next.clone());
    })
}

/// A transcriber that answers each chunk with the next line of `script`.
pub(super) fn scripted_capture(script: &'static [&'static str]) -> RollingCapture {
    let next = AtomicU64::new(0);
    RollingCapture::spawn(move |_| {
        let index = next.fetch_add(1, Ordering::AcqRel) as usize;
        Ok(script.get(index).copied().unwrap_or_default().to_string())
    })
    .expect("rolling")
}
