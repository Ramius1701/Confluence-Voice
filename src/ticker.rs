//! The mixer's 20 ms clock.
//!
//! On Windows a tokio interval wakes on the coarse system timer, about every
//! 31 ms for a 20 ms period (see win_timer.rs), so there the tick comes from a
//! high-resolution waitable timer on its own thread. Elsewhere, and as a fallback,
//! it is tokio's interval.

use std::time::Duration;

pub struct Ticker {
    period: Duration,
    inner: Inner,
}

enum Inner {
    Interval(tokio::time::Interval),
    #[cfg(windows)]
    Thread(tokio::sync::mpsc::Receiver<()>),
}

fn interval(period: Duration) -> Inner {
    let mut i = tokio::time::interval(period);
    i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    Inner::Interval(i)
}

impl Ticker {
    pub fn new(period: Duration) -> Self {
        #[cfg(windows)]
        {
            if let Some(rx) = crate::win_timer::spawn_tick_thread(period) {
                log::info!("mixer clock: high-resolution Windows timer, every {period:?}");
                return Ticker { period, inner: Inner::Thread(rx) };
            }
            log::warn!(
                "mixer clock: no high-resolution Windows timer (needs Windows 10 1803+); \
                 falling back to tokio's interval, which may tick late"
            );
        }
        Ticker { period, inner: interval(period) }
    }

    pub async fn tick(&mut self) {
        match &mut self.inner {
            Inner::Interval(i) => {
                i.tick().await;
            }
            #[cfg(windows)]
            Inner::Thread(rx) => {
                if rx.recv().await.is_none() {
                    // The tick thread has gone; never spin on a closed channel.
                    log::warn!("mixer clock thread stopped; falling back to tokio's interval");
                    self.inner = interval(self.period);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // On Windows this is the check that the mixer is not back on the coarse timer:
    // a plain tokio interval fails it (about 31 ms a tick, so 50 ticks take ~1.55 s).
    #[tokio::test(flavor = "multi_thread")]
    async fn fifty_ticks_take_about_one_second() {
        let mut t = Ticker::new(Duration::from_millis(20));
        t.tick().await;
        let start = Instant::now();
        let mut last = start;
        let mut late = 0;
        for _ in 0..50 {
            t.tick().await;
            let now = Instant::now();
            if now - last > Duration::from_millis(30) {
                late += 1;
            }
            last = now;
        }
        let total = start.elapsed();
        assert!(
            total >= Duration::from_millis(950) && total <= Duration::from_millis(1150),
            "50 ticks of 20 ms took {total:?}"
        );
        // Tests run in parallel, so allow a couple of hiccups.
        assert!(late <= 3, "{late} of 50 ticks came over 10 ms late");
    }
}
