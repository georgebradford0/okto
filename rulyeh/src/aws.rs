use std::time::Duration;

use anyhow::Context;
use tracing::info;

fn region() -> String {
    std::env::var("AWS_DEFAULT_REGION").unwrap_or_else(|_| "us-east-1".to_string())
}

pub async fn describe_latest_ubuntu_ami() -> anyhow::Result<String> {
    let r = region();
    let out = tokio::process::Command::new("aws")
        .args([
            "ec2", "describe-images",
            "--region", &r,
            "--owners", "099720109477",
            "--filters",
            "Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*",
            "Name=state,Values=available",
            "--query", "sort_by(Images, &CreationDate)[-1].ImageId",
            "--output", "text",
        ])
        .output()
        .await
        .context("aws describe-images")?;
    if !out.status.success() {
        anyhow::bail!("aws describe-images: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

pub struct InstanceSpec<'a> {
    pub ami: &'a str,
    pub instance_type: &'a str,
    pub security_group_id: &'a str,
    pub subnet_id: &'a str,
    pub child_name: &'a str,
    pub user_data: &'a str,
}

pub async fn run_instance(spec: &InstanceSpec<'_>) -> anyhow::Result<String> {
    let r = region();
    let tags = format!(
        "ResourceType=instance,Tags=[{{Key=claudulhu.managed,Value=1}},{{Key=claudulhu.child-name,Value={}}}]",
        spec.child_name
    );
    let out = tokio::process::Command::new("aws")
        .args([
            "ec2", "run-instances",
            "--region", &r,
            "--image-id", spec.ami,
            "--instance-type", spec.instance_type,
            "--security-group-ids", spec.security_group_id,
            "--subnet-id", spec.subnet_id,
            "--associate-public-ip-address",
            "--tag-specifications", &tags,
            "--user-data", spec.user_data,
            "--query", "Instances[0].InstanceId",
            "--output", "text",
        ])
        .output()
        .await
        .context("aws run-instances")?;
    if !out.status.success() {
        anyhow::bail!("aws run-instances: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

pub async fn describe_instance(instance_id: &str) -> anyhow::Result<(String, String)> {
    let r = region();
    let out = tokio::process::Command::new("aws")
        .args([
            "ec2", "describe-instances",
            "--region", &r,
            "--instance-ids", instance_id,
            "--query", "Reservations[0].Instances[0].[State.Name,PublicIpAddress]",
            "--output", "json",
        ])
        .output()
        .await
        .context("aws describe-instances")?;
    if !out.status.success() {
        anyhow::bail!("aws describe-instances: {}", String::from_utf8_lossy(&out.stderr));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let state = v[0].as_str().unwrap_or("").to_string();
    let ip    = v[1].as_str().unwrap_or("").to_string();
    Ok((state, ip))
}

pub async fn wait_for_instance_running(instance_id: &str) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("timeout waiting for EC2 instance {instance_id} to be running");
        }
        let (state, ip) = describe_instance(instance_id).await?;
        if state == "running" && !ip.is_empty() {
            info!("[aws] instance {instance_id} running, public IP: {ip}");
            return Ok(ip);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

pub async fn terminate_instance(instance_id: &str) -> anyhow::Result<()> {
    let r = region();
    let out = tokio::process::Command::new("aws")
        .args([
            "ec2", "terminate-instances",
            "--region", &r,
            "--instance-ids", instance_id,
        ])
        .output()
        .await
        .context("aws terminate-instances")?;
    if !out.status.success() {
        anyhow::bail!("aws terminate-instances: {}", String::from_utf8_lossy(&out.stderr));
    }
    info!("[aws] terminated instance {instance_id}");
    Ok(())
}
