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
    let mut stream = super::open_stream(&Request {
        id: "cli:events:subscribe".into(),
        method: Method::EventsSubscribe(params),
    })?;
    let mut line = String::new();
    loop {
        line.clear();
        if stream
            .read_line(&mut line)
            .map_err(super::target::remote_error)?
            == 0
        {
            let closed =
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "event stream closed");
            eprintln!("error: {}", super::target::remote_error(closed));
            return Ok(1);
        }
        let text = line.trim_end();
        if serde_json::from_str::<StreamLine>(text).is_ok_and(|line| line.error.is_some()) {
            eprintln!("{text}");
            return Ok(1);
        }
        println!("{text}");
    }
}
