//! End-to-end spike: drive a real hc core through the sync client.
//!
//! Run: `cargo run -p hc-spike --bin hc-spike`
//! Exercises hello → terminal.list → events.subscribe → terminal.run →
//! send/read echo → event notification → kill/rm cleanup.

use std::time::{Duration, Instant};

use hc_spike::HcClient;

fn main() {
    let mut failures = 0;
    let mut check = |name: &str, ok: bool| {
        println!("{} {}", if ok { "PASS" } else { "FAIL" }, name);
        if !ok {
            failures += 1;
        }
    };

    let client = match HcClient::local() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot spawn hc connect: {e}");
            std::process::exit(2);
        }
    };

    // 1. hello handshake
    let hello = client.hello().expect("hello");
    let version = hello["version"].as_str().unwrap_or("?");
    let methods = hello["methods"].as_array().map(|a| a.len()).unwrap_or(0);
    println!("core version={version} methods={methods}");
    check("hello returns version+methods", methods > 20);

    // 2. terminal.list
    let list = client.terminal_list().expect("terminal.list");
    let n = list["terminals"].as_array().map(|a| a.len()).unwrap_or(0);
    println!("live terminals: {n}");
    check("terminal.list works", true);

    // 3. events: dedicated second connection — in plain (non-framed) mode
    // events.subscribe turns the connection into a notification-only
    // stream, so RPC must keep using `client`.
    let ev_client = HcClient::local().expect("second hc connect");
    let events = ev_client.subscribe_events();
    let sub = ev_client
        .events_subscribe(&[("probe", "hc-spike2")])
        .expect("events.subscribe");
    println!(
        "subscribe seq={} matched={}",
        sub["seq"],
        sub["terminals"].as_array().map(|a| a.len()).unwrap_or(0)
    );

    // 4. terminal.run throwaway
    let term = client
        .terminal_run(
            &["/bin/sh", "-c", "echo READY-SPIKE; exec cat"],
            Some("/tmp"),
            &[("probe", "hc-spike2")],
        )
        .expect("terminal.run");
    let addr = term["address"].as_str().unwrap_or("").to_string();
    println!("spawned terminal {addr}");
    check("terminal.run returns address", addr.starts_with('t'));

    std::thread::sleep(Duration::from_millis(800));

    // 5. read echo
    let read = client.terminal_read(&addr, 20).expect("terminal.read");
    let text = read["text"].as_str().unwrap_or("");
    println!("read: {:?}", &text[..text.len().min(80)]);
    check(
        "terminal.read sees READY-SPIKE",
        text.contains("READY-SPIKE"),
    );

    // 6. send + read back
    client
        .terminal_send(&addr, "hello-from-spike")
        .expect("terminal.send");
    std::thread::sleep(Duration::from_millis(500));
    let read2 = client.terminal_read(&addr, 20).expect("terminal.read#2");
    let text2 = read2["text"].as_str().unwrap_or("");
    check(
        "send+read echo roundtrip",
        text2.contains("hello-from-spike"),
    );

    // 7. event notification for the new terminal (created/updated)
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_event = false;
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(500)) {
            Ok(frame) => {
                let s = frame.to_string();
                if s.contains(&addr) {
                    println!("event: {}", &s[..s.len().min(160)]);
                    saw_event = true;
                    break;
                }
            }
            Err(_) => continue,
        }
    }
    check("events.subscribe delivers terminal event", saw_event);

    // 8. cleanup
    client.terminal_kill_rm(&addr).expect("cleanup");
    println!("cleaned up {addr}");

    println!(
        "\n{}",
        if failures == 0 {
            "ALL PASS"
        } else {
            "FAILURES"
        }
    );
    std::process::exit(if failures == 0 { 0 } else { 1 });
}
