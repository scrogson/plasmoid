#[allow(warnings)]
mod bindings;

use bindings::exports::plasmoid::caller::caller::Guest;
use bindings::plasmoid::runtime::{actor_context, logging};

struct CallerActor;

impl Guest for CallerActor {
    fn call_echo(message: String) -> Result<String, String> {
        logging::log(
            logging::Level::Info,
            &format!("Caller: calling echo with '{}'", message),
        );

        // Wave-encode the string argument (strings are quoted in wave format)
        let wave_arg = format!("\"{}\"", message);

        // Call the echo actor's echo function
        let results = actor_context::call("echo", "echo", &[wave_arg])?;

        // The result is a wave-encoded string — strip the surrounding quotes
        let wave_result = results
            .first()
            .ok_or_else(|| "echo returned no results".to_string())?;

        let unquoted = wave_result
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(wave_result)
            .to_string();

        logging::log(
            logging::Level::Info,
            &format!("Caller: echo returned '{}'", unquoted),
        );

        Ok(unquoted)
    }
}

bindings::export!(CallerActor with_types_in bindings);
