//! Optional hosted feedback uses the existing adapter and its bounded worker, never CLI telemetry.
use clap::Args;
use std::process::{Command, Stdio};

#[derive(Debug, Args)]
pub struct FeedbackArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

pub fn run(args: FeedbackArgs) {
    let Ok(program) = std::env::current_exe() else {
        return;
    };
    let mut source =
        String::from("import os,sys,types\nif sys.version_info < (3, 10): sys.exit(0)\n");
    for (name, contents) in [
        ("clients", include_str!("client_feedback/clients.py")),
        (
            "connections",
            include_str!("client_feedback/connections.py"),
        ),
    ] {
        source.push_str(&format!(
            "m=types.ModuleType('{name}');sys.modules['{name}']=m;exec({},m.__dict__)\n",
            serde_json::to_string(contents).expect("static source")
        ));
    }
    source.push_str(&format!(
        "__file__=os.environ['AGENTSCM_FEEDBACK_EXECUTABLE'];exec({})\n",
        serde_json::to_string(include_str!("client_feedback/feedback.py")).expect("static source")
    ));
    let _ = Command::new("python3")
        .args(["-I", "-B", "-c", &source])
        .args(args.args)
        .env("AGENTSCM_FEEDBACK_EXECUTABLE", program)
        .stderr(Stdio::null())
        .status();
}
