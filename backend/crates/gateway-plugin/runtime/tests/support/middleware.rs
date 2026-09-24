use gateway_plugin_sdk::{
    Capability, ContributionDeclaration, Contributions, Stage,
    client::{MiddlewareCall, MiddlewarePlugin, PluginSession, SessionConfig},
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let plugin = MiddlewarePlugin::new(
        &Contributions::from([(
            Capability::Middleware,
            ContributionDeclaration {
                id: "test.example.middleware".into(),
                version: 1,
                stages: vec![Stage::Request],
                input_formats: vec!["openai".into()],
                output_formats: vec!["openai".into()],
            },
        )]),
        |call: MiddlewareCall| async move {
            let mut response = call.next.run(call.request).await?;
            response.append_header("x-sdk-plugin", b"active".to_vec());
            response.body = response.body.map_frames(|mut frame| {
                frame.payload.push(b' ');
                Ok(vec![frame])
            })?;
            Ok(response)
        },
    )?;
    PluginSession::accept(
        tokio::io::stdin(),
        tokio::io::stdout(),
        SessionConfig::default(),
    )
    .await?
    .run(plugin)
    .await?;
    Ok(())
}
