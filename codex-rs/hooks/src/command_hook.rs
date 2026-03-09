use std::process::Stdio;
use std::sync::Arc;

use serde::Deserialize;

use crate::Hook;
use crate::HookPayload;
use crate::HookResult;
use crate::command_from_argv;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AfterToolUseHookOutput {
    #[serde(default)]
    updated_tool_output: Option<String>,
}

/// Create a hook from an argv specification that:
/// 1. Serializes the full `HookPayload` as JSON
/// 2. Passes it as the last argv argument
/// 3. Waits for process exit
/// 4. Maps exit-code 0 → Success, non-zero → FailedAbort
///
/// This is the general-purpose hook builder used for `before_tool_use`
/// (and any future hook points that need synchronous gate behaviour).
pub fn command_hook(argv: Vec<String>, name: String) -> Hook {
    let argv = Arc::new(argv);
    Hook {
        name,
        func: Arc::new(move |payload: &HookPayload| {
            let argv = Arc::clone(&argv);
            Box::pin(async move {
                let mut command = match command_from_argv(&argv) {
                    Some(command) => command,
                    None => return HookResult::Success,
                };

                // Serialize the full hook payload and append as last argument.
                if let Ok(json) = serde_json::to_string(payload) {
                    command.arg(json);
                }

                command.stdin(Stdio::null()).stdout(Stdio::null());

                match command.output().await {
                    Ok(output) if output.status.success() => HookResult::Success,
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        let code = output.status.code().unwrap_or(-1);
                        HookResult::FailedAbort(
                            std::io::Error::other(format!(
                                "hook exited with code {code}: {stderr}"
                            ))
                            .into(),
                        )
                    }
                    Err(err) => HookResult::FailedContinue(err.into()),
                }
            })
        }),
    }
}

pub fn after_tool_use_command_hook(argv: Vec<String>, name: String) -> Hook {
    let argv = Arc::new(argv);
    Hook {
        name,
        func: Arc::new(move |payload: &HookPayload| {
            let argv = Arc::clone(&argv);
            Box::pin(async move {
                let mut command = match command_from_argv(&argv) {
                    Some(command) => command,
                    None => return HookResult::Success,
                };

                if let Ok(json) = serde_json::to_string(payload) {
                    command.arg(json);
                }

                command.stdin(Stdio::null()).stdout(Stdio::piped());

                match command.output().await {
                    Ok(output) if output.status.success() => {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        if let Ok(parsed) = serde_json::from_str::<AfterToolUseHookOutput>(&stdout)
                            && let Some(modified) = parsed.updated_tool_output
                        {
                            return HookResult::SuccessWithModifiedOutput(modified);
                        }
                        HookResult::Success
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        let code = output.status.code().unwrap_or(-1);
                        HookResult::FailedAbort(
                            std::io::Error::other(format!(
                                "hook exited with code {code}: {stderr}"
                            ))
                            .into(),
                        )
                    }
                    Err(err) => HookResult::FailedContinue(err.into()),
                }
            })
        }),
    }
}
