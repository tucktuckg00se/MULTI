//! `latency delay`: UDP delay line for calibration. Holds every datagram for a
//! fixed time, then forwards it unchanged and in order.

use crate::clock::{now_ns, sleep_until};
use anyhow::{Context, Result};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::channel;
use std::time::Duration;

pub fn run(listen: SocketAddr, forward: SocketAddr, delay_ms: f64, duration: Option<f64>) -> Result<()> {
    let sock = UdpSocket::bind(listen).with_context(|| format!("bind {listen}"))?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    crate::sock::set_rcvbuf(&sock, 8 << 20);
    let out = UdpSocket::bind("0.0.0.0:0")?;
    out.connect(forward).with_context(|| format!("connect {forward}"))?;
    let delay_ns = (delay_ms * 1e6) as i64;
    let (tx, rx) = channel::<(i64, Vec<u8>)>();
    let sender = std::thread::spawn(move || {
        let mut late_max = 0i64;
        for (due, data) in rx {
            sleep_until(due);
            late_max = late_max.max(now_ns() - due);
            let _ = out.send(&data);
        }
        late_max
    });
    eprintln!("delay: {listen} -> {forward}, holding {delay_ms} ms");
    let deadline = duration.map(|d| now_ns() + (d * 1e9) as i64);
    let mut buf = vec![0u8; 65536];
    loop {
        if deadline.is_some_and(|d| now_ns() >= d) {
            break;
        }
        match sock.recv(&mut buf) {
            Ok(n) => {
                let t = now_ns();
                if tx.send((t + delay_ns, buf[..n].to_vec())).is_err() {
                    break;
                }
            }
            Err(_) => continue,
        }
    }
    drop(tx);
    if let Ok(late) = sender.join() {
        eprintln!("delay: worst send lateness {:.3} ms", late as f64 / 1e6);
    }
    Ok(())
}
