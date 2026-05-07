use anyhow::Result;
use octo_k8s_ops::k8s;

pub async fn list() -> Result<()> {
    let client = k8s::build_client().await?;
    let children = k8s::list_managed_deployments(&client).await?;
    if children.is_empty() {
        println!("No pods.");
        return Ok(());
    }
    println!("{:<32} {:<10} {:<6} {}", "NAME", "STATUS", "PORT", "GIT URL");
    println!("{}", "-".repeat(80));
    for c in children {
        println!("{:<32} {:<10} {:<6} {}", c.name, c.status, c.noise_port, c.git_url);
    }
    Ok(())
}

pub async fn create(git_url: Option<&str>, name: Option<&str>, noise_port: Option<u16>) -> Result<()> {
    let client = k8s::build_client().await?;

    let child_name = name.map(str::to_string).unwrap_or_else(|| match git_url {
        Some(u) => format!(
            "lair-{}",
            u.trim_end_matches('/')
                .split('/')
                .last()
                .unwrap_or("repo")
                .trim_end_matches(".git")
                .to_lowercase()
        ),
        None => "lair-workload".to_string(),
    });

    let port = match noise_port {
        Some(p) => p,
        None => k8s::assign_nodeport(&client).await?,
    };

    let api_key   = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    let gh_token  = std::env::var("GH_TOKEN").ok().filter(|s| !s.is_empty());
    let pub_host  = std::env::var("PUBLIC_HOST").unwrap_or_default();
    let noise_key = k8s::read_secret_value(&client, "lair-secrets", "NOISE_PRIVATE_KEY")
        .await
        .unwrap_or_default();

    let params = k8s::CreateChildParams {
        name:              &child_name,
        git_url,
        noise_port:        port,
        api_key:           &api_key,
        gh_token:          gh_token.as_deref(),
        pub_host:          &pub_host,
        lair_url:        "",
        startup_script:    None,
        startup_prompt:    None,
        noise_private_key: &noise_key,
    };

    k8s::create_child_resources(&client, &params).await?;
    println!("Created '{child_name}' on NodePort {port}.");
    Ok(())
}

pub async fn start(name: &str) -> Result<()> {
    let client = k8s::build_client().await?;
    k8s::scale_deployment(&client, name, 1).await?;
    println!("Started '{name}'.");
    Ok(())
}

pub async fn stop(name: &str) -> Result<()> {
    let client = k8s::build_client().await?;
    k8s::scale_deployment(&client, name, 0).await?;
    println!("Stopped '{name}'.");
    Ok(())
}

pub async fn delete(name: &str, yes: bool) -> Result<()> {
    if !yes {
        use std::io::Write;
        print!("Delete '{name}' and all its data? This is irreversible. [y/N] ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let trimmed = input.trim().to_lowercase();
        if trimmed != "y" && trimmed != "yes" {
            println!("Aborted.");
            return Ok(());
        }
    }
    let client = k8s::build_client().await?;
    k8s::delete_child_resources(&client, name).await?;
    println!("Deleted '{name}'.");
    Ok(())
}

