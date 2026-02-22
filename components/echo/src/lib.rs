#[derive(Default)]
struct Echo;

#[plasmoid_sdk::gen_server]
impl Echo {
    fn handle_call(&mut self, req: Vec<u8>) -> Vec<u8> {
        req
    }

    fn handle_info(&mut self, data: Vec<u8>) -> plasmoid_sdk::CastResult {
        if data == b"stop" {
            return plasmoid_sdk::CastResult::Stop;
        }
        plasmoid_sdk::CastResult::Continue
    }
}
