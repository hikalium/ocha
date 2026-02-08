
struct RunTurnConfig<'a> {
    client: &'a reqwest::Client,
    url: &'a str,
    model: &'a str,
    initial_prompt: &'a str,
    session: &'a mut Session,
    reminders: &'a [Reminder],
    command_per_response: usize,
    is_new_session: bool,
}

async fn run_turn(config: RunTurnConfig<'_>) -> Result<(), Box<dyn std::error::Error>> {
    let mut current_prompt = apply_reminders(config.initial_prompt, config.reminders, config.is_new_session);
    let mut remaining_commands = config.command_per_response;

    loop {
        // If we are continuing a chain of commands, we suppress stdout streaming until we are sure it's not another command
        // But for the user experience, we only want to stream the *final* text response or show some indicator.
        // For simplicity: stream_output is true if we expect a user-facing response.
        // However, since we don't know if the NEXT response is a command or text, we can use the "is_command_mode" logic inside generate_internal.
        
        let response = generate_internal(config.client, config.url, config.model, &current_prompt, config.session, true).await?;

        if let Some(cmd_json_str) = response.trim().strip_prefix("!!!OCHA_RUN_CMD") {
             if remaining_commands == 0 {
                let error_result = CommandResult {
                    status: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    remaining_commands: 0,
                    error: Some("Command execution limit exceeded per response.".to_string()),
                };
                 current_prompt = serde_json::to_string(&error_result)?;
                 // Continue loop to report error to model
            } else {
                 remaining_commands -= 1;
                 // Parse JSON
                 match serde_json::from_str::<CommandRequest>(cmd_json_str) {
                     Ok(req) => {
                         println!("[Executing: {} {}]", req.binary, req.args.join(" "));
                         let (status, stdout, stderr, error) = execute_command(req).await;
                         let result = CommandResult {
                             status,
                             stdout,
                             stderr,
                             remaining_commands,
                             error,
                         };
                         current_prompt = serde_json::to_string(&result)?;
                     }
                     Err(e) => {
                         let error_result = CommandResult {
                             status: None,
                             stdout: String::new(),
                             stderr: String::new(),
                             remaining_commands,
                             error: Some(format!("Failed to parse command request: {}", e)),
                         };
                         current_prompt = serde_json::to_string(&error_result)?;
                     }
                 }
            }
        } else {
            // Normal text response, we are done with this turn
            break;
        }
    }

    Ok(())
}
