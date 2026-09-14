//! CLI for the intentional pane-write protocol.
use super::*;
use prismattyc_mux::PaneWriteSubmit;

const HELP: &str =
    "pmux pane-write PANE (--text TEXT | --stdin) [--submit auto|enter|none] [--json]

Write literal text to exactly one pane ID. No sync-input fan-out or lease takeover.
The default auto mode pastes into a detected agent and sends its submit sequence.
Use enter to append CR, or none to leave text unsubmitted. A busy or dirty pane
refuses the write. --stdin reads UTF-8 text. Maximum text size: 65504 bytes.
JSON receipts report queued bytes, not recipient acceptance. A partial write
exits nonzero. Never automatically retry a partial write or a lost response.
";

#[derive(Debug)]
struct Args {
    pane: u64,
    text: String,
    submit: PaneWriteSubmit,
    json: bool,
}

fn parse(rest: Vec<String>) -> Result<Args> {
    let mut args = rest.into_iter();
    let pane = args
        .next()
        .context(HELP)?
        .parse::<u64>()
        .context("PANE must be a numeric pane ID")?;
    if pane == 0 {
        bail!("PANE must be positive");
    }
    let mut text = None;
    let mut stdin = false;
    let mut json = false;
    let mut submit = PaneWriteSubmit::Auto;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--text" if text.is_none() => text = Some(args.next().context("--text requires TEXT")?),
            "--stdin" if !stdin => stdin = true,
            "--json" => json = true,
            "--submit" => {
                submit = match args.next().as_deref() {
                    Some("auto") => PaneWriteSubmit::Auto,
                    Some("enter") => PaneWriteSubmit::Enter,
                    Some("none") => PaneWriteSubmit::None,
                    _ => bail!("--submit requires auto, enter, or none"),
                }
            }
            _ => bail!("unknown or repeated pane-write argument {arg:?}"),
        }
    }
    if stdin == text.is_some() {
        bail!("choose exactly one of --text or --stdin");
    }
    let text = if stdin {
        let mut text = String::new();
        io::stdin()
            .take(65505)
            .read_to_string(&mut text)
            .context("read UTF-8 stdin")?;
        text
    } else {
        text.unwrap_or_default()
    };
    if text.is_empty() || text.len() > 65504 {
        bail!("text must contain 1..65504 UTF-8 bytes");
    }
    Ok(Args {
        pane,
        text,
        submit,
        json,
    })
}

pub(super) fn run(paths: &Paths, rest: Vec<String>) -> Result<()> {
    if rest.first().is_some_and(|s| s == "--help" || s == "-h") {
        print!("{HELP}");
        return Ok(());
    }
    // Remember JSON mode before parsing so protocol failures remain structured.
    let json = rest.iter().any(|s| s == "--json");
    let result = parse(rest).and_then(|args| execute(paths, args));
    if let Err(error) = &result {
        if json {
            let code = control_code(error);
            println!(
                "{}",
                serde_json::json!({"status":"error", "code":code,
                "message":format!("{error:#}")})
            );
        }
    }
    match result {
        Ok(true) => Ok(()),
        Ok(false) => bail!("partial pane write; inspect the recipient before continuing"),
        Err(error) => Err(error),
    }
}

fn execute(paths: &Paths, args: Args) -> Result<bool> {
    require_live_socket(paths)?;
    let (mut client, client_id) = connect_registered(paths)?;
    let snapshot = take_snapshot(&mut client)?;
    let expected_child_pid = snapshot_pane(&snapshot, args.pane)
        .and_then(|pane| pane.child_pid)
        .context("pane is missing or has no live child")?;
    let response = client.request(|request_id| ControlRequest::PaneWrite {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id: args.pane,
        expected_child_pid,
        data: args.text,
        submit: args.submit,
    })?;
    let ControlResponseData::PaneWriteResult {
        complete,
        nbytes,
        total_bytes,
        ..
    } = &response
    else {
        bail!("daemon returned an unexpected pane-write response");
    };
    if args.json {
        println!(
            "{}",
            serde_json::json!({"status":if *complete {"queued"} else {"partial"}, "response":response})
        );
    } else {
        println!(
            "queued {nbytes}/{total_bytes} bytes to pane {}{}",
            args.pane,
            if *complete {
                ""
            } else {
                "; partial write: do not retry automatically"
            }
        );
    }
    Ok(*complete)
}
