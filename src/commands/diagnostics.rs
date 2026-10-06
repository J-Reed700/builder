use anyhow::{Context, Result, ensure};
use builder::ui;
use builder_core::{config::Profile, store::Store};
use builder_provider::OpenAiCompatible;
use std::path::Path;

pub(crate) async fn run(
    doctor: bool,
    home: &Path,
    profile_name: &str,
    profile: &Profile,
    provider: &OpenAiCompatible,
    store: &mut Store,
) -> Result<()> {
    if doctor {
        println!(
            "✓ configuration valid\n✓ durable storage: {}\n✓ profile: {}\n  endpoint: {}",
            home.display(),
            ui::safe(profile_name),
            profile.base_url
        );
    }
    let models = provider.models().await.context(
        "Model discovery failed. Check the URL (usually ending in /v1), credentials, and whether the server exposes GET /models",
    )?;
    let Some(models) = models["data"].as_array() else {
        println!("{}", ui::safe(&serde_json::to_string_pretty(&models)?));
        return Ok(());
    };

    for model in models {
        if let Some(id) = model["id"].as_str() {
            println!("{}", ui::safe(id));
        }
    }
    if !doctor {
        return Ok(());
    }

    if models
        .iter()
        .any(|model| model["id"].as_str() == Some(&profile.model))
    {
        println!("✓ configured model is advertised");
    } else {
        println!(
            "! configured model '{}' was not advertised; verify its ID",
            ui::safe(&profile.model)
        );
    }
    println!("Running active generation, JSON, and tool-call probes…");
    let conformance = builder::doctor::conformance(provider, profile.tools).await;
    let conformance_value = serde_json::to_value(&conformance)?;
    let conformance_identity = serde_json::to_vec(&(
        &profile.base_url,
        &profile.model,
        profile.stream,
        profile.tools,
        profile.context_tokens,
        profile.max_output_tokens,
        profile.max_attempts,
        profile.connect_timeout_secs,
        profile.idle_timeout_secs,
        profile.request_timeout_secs,
        &profile.completion,
    ))?;
    store.save_provider_conformance(
        &builder_core::memory::digest(&conformance_identity),
        profile_name,
        &profile.base_url,
        &profile.model,
        &conformance_value,
    )?;
    println!(
        "Active conformance (saved by endpoint/model fingerprint):\n{}",
        ui::safe(&serde_json::to_string_pretty(&conformance_value)?)
    );
    let failures = conformance.required_failures(profile.tools, profile.pipeline.parallel_tools);
    ensure!(
        failures.is_empty(),
        "Required provider conformance probes failed: {}",
        failures.join(", ")
    );
    Ok(())
}
