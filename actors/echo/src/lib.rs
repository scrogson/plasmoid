#[allow(warnings)]
mod bindings;

use bindings::exports::plasmoid::echo::echo::Guest;
use bindings::plasmoid::runtime::logging;

struct EchoActor;

impl Guest for EchoActor {
    fn echo(message: String) -> String {
        logging::log(
            logging::Level::Info,
            &format!("Echo: {}", message),
        );
        message
    }

    fn reverse(message: String) -> String {
        logging::log(
            logging::Level::Info,
            &format!("Reverse: {}", message),
        );
        message.chars().rev().collect()
    }
}

bindings::export!(EchoActor with_types_in bindings);
