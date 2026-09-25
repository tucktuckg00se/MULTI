//! Shared clock: CLOCK_MONOTONIC in nanoseconds.
//!
//! Every tap on the same host reads the same system-wide monotonic clock, so
//! wallclock values from different `latency tap` processes are directly
//! comparable. (Not comparable across hosts.)

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

unsafe extern "C" {
    fn clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32;
}

/// Linux `CLOCK_MONOTONIC`.
const CLOCK_MONOTONIC: i32 = 1;

/// Nanoseconds on CLOCK_MONOTONIC. Returns 0 if the call fails (it cannot on Linux).
pub fn now_ns() -> i64 {
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec with the C layout for x86-64 Linux.
    let rc = unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

/// Sleeps until `deadline_ns` on the monotonic clock: coarse sleep, then a short spin
/// so the wake-up error stays in the tens of microseconds.
pub fn sleep_until(deadline_ns: i64) {
    const SPIN_NS: i64 = 300_000;
    loop {
        let left = deadline_ns - now_ns();
        if left <= 0 {
            return;
        }
        if left > SPIN_NS {
            std::thread::sleep(std::time::Duration::from_nanos((left - SPIN_NS) as u64));
        } else {
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn monotonic() {
        let a = super::now_ns();
        let b = super::now_ns();
        assert!(a > 0 && b >= a);
    }
}
