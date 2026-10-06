use std::io::BufRead;

use crate::api::schema::{EventsSubscribeParams, Method, Request};

const USAGE: &str = "usage: herdr events subscribe --json <PARAMS>";

pub(super) fn run_events_command(args: &[String]) -> std::io::Result<i32> {
    match args {
        [subcommand, flag, params] if subcommand == "subscribe" && flag == "--json" => {
            subscribe(params)
        }
        _ => {
            eprintln!("{USAGE}");
            Ok(2)
        }
    }
}

/// The top-level `error` key marks the one error line that ends a stream.
#[derive(serde::Deserialize)]
struct StreamLine {
    error: Option<serde::de::IgnoredAny>,
}

fn subscribe(params: &str) -> std::io::Result<i32> {
    let params: EventsSubscribeParams = match serde_json::from_str(params) {
        Ok(params) => params,
        Err(error) => {
            eprintln!("invalid --json params: {error}");
            return Ok(2);
        }
    };
    let request = Request {
        id: "cli:events:subscribe".into(),
        method: Method::EventsSubscribe(params),
    };
    let mut stream = match super::open_stream(&request) {
        Ok(stream) => stream,
        // These were already reported as JSON on stderr, or main prints them so.
        Err(err)
            if super::protocol_mismatch_was_reported(&err)
                || super::server_not_running_was_reported(&err) =>
        {
            return Err(err)
        }
        Err(err) => {
            eprintln!("error: {err}");
            return Ok(1);
        }
    };
    let mut released = false;
    let mut line = String::new();
    loop {
        line.clear();
        let read = stream.read_line(&mut line);
        if !released {
            // The server answered, so the SSH child read its config and the stream
            // needs neither the bridge socket nor the config any more.
            super::target::release_machine_files();
            released = true;
        }
        let error = match read {
            Ok(0) => std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "event stream closed"),
            Ok(_) => {
                let text = line.trim_end();
                if serde_json::from_str::<StreamLine>(text).is_ok_and(|line| line.error.is_some()) {
                    eprintln!("{text}");
                    return Ok(1);
                }
                println!("{text}");
                continue;
            }
            Err(err) => err,
        };
        eprintln!("error: {}", super::target::remote_error(error));
        return Ok(1);
    }
}
