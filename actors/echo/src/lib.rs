wit_bindgen::generate!({
    world: "actor",
    path: "../../wit",
});

struct EchoActor;

impl exports::plasmoid::actor::handler::Guest for EchoActor {
    fn handle(request: Vec<u8>) -> Result<Vec<u8>, String> {
        // Log the request
        plasmoid::actor::logging::log(
            plasmoid::actor::logging::Level::Info,
            &format!("Echo actor received {} bytes", request.len()),
        );

        // Echo back the request
        Ok(request)
    }
}

export!(EchoActor);
