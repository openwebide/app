use openwebide_plugin_sdk::{Plugin, Tool, Outcome, serde_json::{self, json}};
struct Fixture;
impl Plugin for Fixture {
    fn tools() -> Vec<Tool> {
        vec![Tool {
            name: "fixture_echo".into(), description: "Exercise the public host contract.".into(),
            parameters: json!({"type":"object","properties":{}}), requires_approval: false,
        }]
    }
    fn execute(name: &str, arguments: serde_json::Value) -> Result<Outcome, String> {
        match name {
            "fixture_echo" => {
                let response: serde_json::Value = openwebide_plugin_sdk::request("records", &arguments)?;
                Ok(Outcome {ok: true, content: response.to_string(), summary: "Host capability response".into()})
            }
            "fixture_panic" => panic!("fixture trap"),
            "fixture_loop" => loop { std::hint::black_box(1); },
            "fixture_large_input" => {
                let response = openwebide_plugin_sdk::HttpResponse {
                    status: 200, headers: Default::default(),
                    body_base64: arguments["encoded"].as_str().ok_or("Missing encoded input")?.into(),
                };
                let mut bytes = response.bytes()?;
                bytes.truncate(16_384);
                Ok(Outcome {ok: true, content: String::from_utf8(bytes).map_err(|error| error.to_string())?, summary: "Decoded bounded response".into()})
            }
            _ => Err("Unknown fixture tool".into()),
        }
    }
}
openwebide_plugin_sdk::export!(Fixture);
