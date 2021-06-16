use std::borrow::Cow;
use std::cmp;
use std::collections::HashMap;
use std::io::prelude::*;
use std::path::Path;
use std::{fs, io};
use structopt::StructOpt;
// https://github.com/clap-rs/clap/pull/1397
#[macro_use]
extern crate clap;
use anyhow::{bail, Context, Result};
use directories;
use lazy_static::lazy_static;
use serde_derive::{Deserialize, Serialize};
use sys_info::{cpu_num, mem_info};
use tabwriter::TabWriter;

mod config;

lazy_static! {
    static ref APPDIRS: directories::ProjectDirs =
        directories::ProjectDirs::from("org", "openshift", "xokdinst").expect("creating appdirs");
}

// Stolen from https://github.com/openshift/installer/commit/e26c1989707927018631213e63528d4d0a08e793
static SINGLE_MASTER_CONFIGS: [&'static str; 2] = [
    r###"
apiVersion: operator.openshift.io/v1
kind: Etcd
metadata:
  name: cluster
spec:
  managementState: Managed
  unsupportedConfigOverrides:
    useUnsupportedUnsafeNonHANonProductionUnstableEtcd: true
"###,
    r###"
apiVersion: operator.openshift.io/v1
kind: IngressController
metadata:
  name: default
  namespace: openshift-ingress-operator
spec:
  replicas: 1
"###,
];

static LAUNCHED_CONFIG_PATH: &str = "xokdinst-launched-config.yaml";
static FAILED_STAMP_PATH: &str = "xokdinst-failed";
static KUBECONFIG_PATH: &str = "auth/kubeconfig";
static METADATA_PATH: &str = "metadata.json";
/// Relative path in home to credentials used to authenticate to registries
/// The podman stack uses a different path by default but will honor this
/// one if it exists.
static DOCKERCFG_PATH: &str = ".docker/config.json";

/// Holds extra keys from a map we didn't explicitly parse
type SerdeYamlMap = HashMap<String, serde_yaml::Value>;

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
enum InstallConfigPlatform {
    Libvirt(SerdeYamlMap),
    AWS(SerdeYamlMap),
    GCP(SerdeYamlMap),
    AZURE(SerdeYamlMap),
}

#[derive(Serialize, Deserialize, Clone)]
#[allow(dead_code)]
struct InstallConfigMachines {
    name: String,
    replicas: u32,

    #[serde(flatten)]
    extra: Option<SerdeYamlMap>,
}

#[derive(Serialize, Deserialize, Clone)]
#[allow(dead_code)]
struct InstallConfigMetadata {
    name: String,

    #[serde(flatten)]
    extra: Option<SerdeYamlMap>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct InstallConfig {
    api_version: String,
    base_domain: String,
    compute: Vec<InstallConfigMachines>,
    control_plane: InstallConfigMachines,
    metadata: Option<InstallConfigMetadata>,
    platform: InstallConfigPlatform,
    #[serde(skip_serializing_if = "Option::is_none")]
    pull_secret: Option<String>,
    ssh_key: Option<String>,

    #[serde(flatten)]
    extra: SerdeYamlMap,
}

/// State used to launch a cluster
#[derive(Serialize, Deserialize)]
struct LaunchedConfig {
    config: InstallConfig,
    // Subset of LaunchOpts
    installer_version: Option<String>,
    release_image: Option<String>,
    boot_image: Option<String>,
    libvirt_auto_size: bool,
}

arg_enum! {
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    enum Platform {
        Libvirt,
        AWS,
        GCP,
        AZURE,
    }
}

arg_enum! {
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    enum ClusterSize {
        Single, // Only install a single node
        Compact, // 3 schedulable masters
    }
}

impl InstallConfigPlatform {
    fn to_platform(&self) -> Platform {
        match self {
            InstallConfigPlatform::Libvirt(_) => Platform::Libvirt,
            InstallConfigPlatform::AWS(_) => Platform::AWS,
            InstallConfigPlatform::GCP(_) => Platform::GCP,
            InstallConfigPlatform::AZURE(_) => Platform::AZURE,
        }
    }
}

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab-case")]
struct LaunchOpts {
    /// Name of the cluster to launch
    name: String,

    #[structopt(short = "D")]
    /// Delete an existing cluster, if one exists
    destroy: bool,

    #[structopt(
        short = "p",
        raw(possible_values = "&Platform::variants()", case_insensitive = "true")
    )]
    platform: Option<Platform>,

    #[structopt(short = "c", long = "config")]
    /// The name of the base configuration (overrides platform)
    config: Option<String>,

    /// Keep the bootstrap image (for development/testing)
    #[structopt(short = "K", long = "keep-bootstrap")]
    keep_bootstrap: bool,

    /// Override the release image (for development/testing)
    #[structopt(short = "I", long = "release-image")]
    release_image: Option<String>,

    /// Use latest release image from stream (for development/testing)
    /// For example, 4.5.0-0.nightly
    /// See openshift-release.svc.ci.openshift.org/ for more information
    #[structopt(short = "S", long = "release-image-from-stream")]
    release_stream: Option<String>,

    /// Override the RHCOS bootimage image (for development/testing)
    #[structopt(short = "O", long = "boot-image")]
    boot_image: Option<String>,

    /// Inject objects during installation (e.g. MachineConfig)
    /// See https://github.com/openshift/installer/blob/master/docs/user/customization.md#kubernetes-customization-unvalidated
    #[structopt(long = "manifests")]
    manifests: Option<String>,

    /// Cluster size
    #[structopt(
        long,
        raw(
            possible_values = "&ClusterSize::variants()",
            case_insensitive = "true"
        )
    )]
    size: Option<ClusterSize>,

    #[structopt(flatten)]
    install_run_opts: InstallRunOpts,
}

#[derive(Debug, StructOpt)]
struct GenConfigOpts {
    /// Name for this configuration; if not specified, will be named config-<platform>.yaml
    name: Option<String>,
    /// Overwrite an existing default configuration
    #[structopt(long)]
    overwrite: bool,

    #[structopt(short = "V", long = "instversion")]
    /// Use a versioned installer binary
    installer_version: Option<String>,

    /// Discover available cpu and memory resources for libvirt
    #[structopt(long)]
    libvirt_auto_size: bool,
}

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab-case")]
struct InstallRunOpts {
    #[structopt(short = "V", long = "instversion")]
    /// Use a versioned installer binary
    installer_version: Option<String>,

    /// Discover available cpu and memory resources for libvirt
    #[structopt(long)]
    libvirt_auto_size: bool,

    /// Enable debug logging from installer
    #[structopt(long)]
    log_debug: bool,
}

#[derive(Debug, StructOpt)]
#[structopt(name = "xokdinst", about = "Extended OpenShift installer wrapper")]
#[structopt(rename_all = "kebab-case")]
/// Main options struct
enum Opt {
    /// Generate the default configuration for a given platform
    GenConfig(GenConfigOpts),
    /// List all configuration sources
    ListConfigs,
    /// List all clusters
    List,
    /// Launch a new cluster
    Launch(LaunchOpts),
    Kubeconfig {
        /// Print the kubeconfig path for this cluster
        name: String,
    },
    /// Destroy a running cluster
    Destroy {
        /// Name of cluster to destroy
        name: String,

        /// Ignore failure to delete cluster, remove directory anyways
        #[structopt(short = "f", long = "force")]
        force: bool,

        #[structopt(flatten)]
        install_run_opts: InstallRunOpts,
    },
}

/// Compute master memory: host memory - (4GB * clusterSize + 2GB for bootstrap), minimum of 4GB
fn get_master_mem(libvirt_auto_size: bool, size: u64) -> u64 {
    let default = 4096;
    if !libvirt_auto_size {
        return default;
    }
    match mem_info() {
        Err(e) => {
            eprintln!(
                "Failed to get total host memory, falling back to {}MB: {}",
                default, e
            );
            return default;
        }
        Ok(mem) => cmp::max(4096, (mem.avail - (4 * size + 2) * 1024 * 1024) / 1024),
    }
}

/// Compute master cpu: host cpu - 2 * clusterSize, minimum of 2
fn get_master_cpu(libvirt_auto_size: bool, size: u32) -> u32 {
    let default = 2;
    if !libvirt_auto_size {
        return default;
    }
    match cpu_num() {
        Err(e) => {
            eprintln!(
                "Failed to get total host cpu, falling back to {}cpu: {}",
                default, e
            );
            return default;
        }
        Ok(cpu) => cmp::max(2, cpu - (2 * size)),
    }
}

fn cmd_installer(
    version: Option<&str>,
    libvirt_auto_size: bool,
    size: Option<&ClusterSize>,
) -> std::process::Command {
    let path = if let Some(version) = version {
        Cow::Owned(format!("openshift-install-{}", version))
    } else {
        Cow::Borrowed("openshift-install")
    };
    let size: u32 = match size.unwrap_or(&ClusterSize::Single) {
        ClusterSize::Single => 1,
        ClusterSize::Compact => 3,
    };
    let mut cmd = std::process::Command::new(path.as_ref());
    // https://github.com/openshift/installer/pull/1890
    cmd.env("OPENSHIFT_INSTALL_INVOKER", "xokdinst");
    // Override the libvirt defaults
    // https://github.com/openshift/installer/pull/785
    cmd.env(
        "TF_VAR_libvirt_master_memory",
        get_master_mem(libvirt_auto_size, size.into()).to_string(),
    )
    .env(
        "TF_VAR_libvirt_master_vcpu",
        get_master_cpu(libvirt_auto_size, size).to_string(),
    );
    cmd
}

fn run_installer(cmd: &mut std::process::Command) -> Result<()> {
    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("Executing openshift-install").context(e))?;
    if !status.success() {
        bail!("openshift-install failed")
    }
    Ok(())
}

/// Return the filename for a given config, using platform name if necessary
fn get_config_name(name: &Option<String>, platform: &Platform) -> String {
    if let Some(name) = name {
        format!("config-{}.yaml", name)
    } else {
        format!("config-{}.yaml", platform.to_string().to_lowercase())
    }
}

/// Create a config-X.yaml
fn generate_config(o: GenConfigOpts) -> Result<String> {
    if let Some(name) = o.name.as_ref() {
        let path = APPDIRS.config_dir().join(&name);
        if !o.overwrite && path.exists() {
            bail!(
                "Configuration '{}' already exists and overwrite not specified",
                name
            );
        }
    }

    let tmpd = tempfile::Builder::new().prefix("xokdinst").tempdir()?;
    println!("Executing `openshift-install create install-config`");
    let mut cmd = cmd_installer(
        o.installer_version.as_ref().map(|s| s.as_str()),
        o.libvirt_auto_size,
        None,
    );
    cmd.args(&["create", "install-config", "--dir"]);
    cmd.arg(tmpd.path());
    run_installer(&mut cmd)?;

    let tmp_config_path = tmpd.path().join("install-config.yaml");
    let mut parsed_config: InstallConfig =
        serde_yaml::from_reader(io::BufReader::new(fs::File::open(tmp_config_path)?))?;
    let platform = parsed_config.platform.to_platform();
    let name = get_config_name(&o.name, &platform);
    let path = APPDIRS.config_dir().join(&name);
    if !o.overwrite && path.exists() {
        bail!(
            "Configuration '{}' already exists and overwrite not specified",
            name
        );
    }

    // Remove the metadata/name value, we want this one to be a template
    match parsed_config.metadata.take() {
        None => bail!("Didn't find expected metadata key"),
        Some(_) => {}
    };

    fs::create_dir_all(APPDIRS.config_dir())?;
    let mut w = io::BufWriter::new(fs::File::create(path)?);
    serde_yaml::to_writer(&mut w, &parsed_config)?;
    w.flush()?;

    Ok(name)
}

/// Get all configurations
fn get_configs() -> Result<Vec<String>> {
    let mut r = Vec::new();
    for entry in fs::read_dir(APPDIRS.config_dir())? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with("config-") && name.ends_with(".yaml") {
                r.push(String::from(name));
            }
        }
    }
    Ok(r)
}

fn get_launched_config<P>(clusterdir: P) -> Result<Option<LaunchedConfig>>
where
    P: AsRef<std::path::Path>,
{
    let clusterdir = clusterdir.as_ref();
    let path = clusterdir.join(LAUNCHED_CONFIG_PATH);
    let config: Option<LaunchedConfig> = if path.exists() {
        let mut r = io::BufReader::new(fs::File::open(path)?);
        Some(serde_yaml::from_reader(&mut r)?)
    } else {
        None
    };
    Ok(config)
}

/// Get all cluster names
fn get_clusters() -> Result<Vec<String>> {
    let mut r = Vec::new();
    for entry in fs::read_dir(APPDIRS.config_dir())? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if !meta.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            r.push(String::from(name));
        }
    }
    Ok(r)
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct LatestRelease {
    name: String,
    #[serde(rename = "pullSpec")]
    pull_spec: String,
    #[serde(rename = "downloadURL")]
    download_url: String,
}

fn get_latest_release(release_stream: &str) -> Result<String> {
    let url = url::Url::parse(
        format!(
            "https://openshift-release.apps.ci.l2s4.p1.openshiftapps.com/api/v1/releasestream/{}/latest",
            release_stream
        )
        .as_str(),
    )
    .expect("parsing url");
    let resp: LatestRelease = reqwest::blocking::get(url)?.json()?;
    Ok(resp.pull_spec)
}

fn print_clusters() -> Result<()> {
    let clusters = get_clusters()?;
    if clusters.len() == 0 {
        println!("No clusters.");
    } else {
        let mut tw = TabWriter::new(std::io::stdout());
        tw.write("NAME\tPLATFORM\tSTATUS\n".as_bytes())?;
        for v in clusters.iter() {
            let clusterdir = APPDIRS.config_dir().join(v.as_str());
            let config = get_launched_config(&clusterdir)?;
            let platform = if let Some(ref config) = config {
                Cow::Owned(config.config.platform.to_platform().to_string())
            } else {
                Cow::Borrowed("<unknown>")
            };
            let failed = clusterdir.join(FAILED_STAMP_PATH).exists();
            let has_kubeconfig = clusterdir.join(KUBECONFIG_PATH).exists();
            let state = if failed {
                "install-failed"
            } else if has_kubeconfig {
                "launched"
            } else {
                "unknown"
            };
            tw.write(format!("{}\t{}\t{}\n", v, platform, state).as_bytes())?;
        }
        tw.flush()?;
    }
    Ok(())
}

fn print_list(header: &str, l: &[String]) {
    if l.len() == 0 {
        println!("No {}.", header);
    } else {
        for v in l.iter() {
            println!("  {}", v);
        }
    }
}

fn cmd_launch_installer(o: &LaunchOpts) -> std::process::Command {
    let installer_version = o
        .install_run_opts
        .installer_version
        .as_ref()
        .map(|x| x.as_str());
    let mut cmd = cmd_installer(
        installer_version,
        o.install_run_opts.libvirt_auto_size,
        o.size.as_ref(),
    );
    if let Some(ref image) = o.release_image {
        cmd.env("OPENSHIFT_INSTALL_RELEASE_IMAGE_OVERRIDE", image);
    }
    if let Some(ref image) = o.boot_image {
        cmd.env("OPENSHIFT_INSTALL_OS_IMAGE_OVERRIDE", image);
    }
    if o.keep_bootstrap {
        cmd.env("OPENSHIFT_INSTALL_PRESERVE_BOOTSTRAP", "true");
    }
    cmd
}

/// 🚀
fn launch(mut o: LaunchOpts) -> Result<()> {
    fs::create_dir_all(APPDIRS.config_dir())?;
    let clusterdir = APPDIRS.config_dir().join(&o.name);
    if clusterdir.exists() {
        if !o.destroy {
            bail!(
                "Cluster {} already exists, use destroy to remove it",
                o.name.as_str()
            );
        }
        destroy(&o.name, true, o.install_run_opts.log_debug)?;
    }
    let configs = get_configs()?;
    let config_name = if o.config.is_none() && configs.len() == 0 {
        println!("No configurations found; generating one now");
        Cow::Owned(generate_config(GenConfigOpts {
            name: None,
            overwrite: false,
            installer_version: o.install_run_opts.installer_version.clone(),
            libvirt_auto_size: false,
        })?)
    } else if let Some(platform) = o.platform.as_ref() {
        Cow::Owned(get_config_name(&None, &platform))
    } else {
        if let Some(name) = o.config.as_ref() {
            Cow::Borrowed(name)
        } else if configs.len() == 1 {
            Cow::Borrowed(&configs[0])
        } else {
            bail!("Have multiple configs, and no config specified")
        }
    };
    let full_name = Cow::Owned(format!("config-{}.yaml", config_name));
    let config_path: Option<_> = [&config_name, &full_name]
        .iter()
        .map(|c| APPDIRS.config_dir().join(c.as_str()))
        .filter(|c| c.exists())
        .next();
    let config_path = match config_path {
        Some(x) => x,
        None => bail!("No such configuration: {}", config_name),
    };

    if let Some(ref stream) = o.release_stream {
        let image = get_latest_release(stream)?;
        println!("Using release image: {}", image);
        o.release_image = Some(image);
    }

    let mut config: InstallConfig =
        serde_yaml::from_reader(io::BufReader::new(fs::File::open(config_path)?))?;
    config.metadata = Some(InstallConfigMetadata {
        name: o.name.to_string(),
        extra: None,
    });

    if let Some(clustersize) = o.size.as_ref() {
        config.control_plane.replicas = match clustersize {
            ClusterSize::Single => 1,
            ClusterSize::Compact => 3,
        };
        config.compute = vec![InstallConfigMachines {
            name: "worker".to_string(),
            replicas: 0,
            extra: Default::default(),
        }];
    }

    // If there's no pull secret, automatically use ~/.docker/config.json
    if config.pull_secret.is_none() {
        let dirs = match directories::BaseDirs::new() {
            Some(x) => x,
            None => bail!("No HOME found"),
        };
        let dockercfg_path = dirs.home_dir().join(DOCKERCFG_PATH);
        if !dockercfg_path.exists() {
            bail!(
                "No pull secret in install config, and no {} found",
                DOCKERCFG_PATH
            );
        }
        let pull_secret = std::fs::read_to_string(dockercfg_path)?;
        config.pull_secret = Some(pull_secret);
    }

    fs::create_dir(&clusterdir)?;
    let mut w = io::BufWriter::new(fs::File::create(clusterdir.join(LAUNCHED_CONFIG_PATH))?);
    let launched_config = LaunchedConfig {
        config: config.clone(),
        installer_version: o.install_run_opts.installer_version.clone(),
        release_image: o.release_image.clone(),
        boot_image: o.boot_image.clone(),
        libvirt_auto_size: o.install_run_opts.libvirt_auto_size,
    };
    serde_yaml::to_writer(&mut w, &launched_config)?;
    w.flush()?;

    let mut w = io::BufWriter::new(fs::File::create(clusterdir.join("install-config.yaml"))?);
    serde_yaml::to_writer(&mut w, &config)?;
    w.flush()?;

    let mut cmd = cmd_launch_installer(&o);
    cmd.arg("version");
    run_installer(&mut cmd)?;

    {
        // https://github.com/openshift/installer/blob/master/docs/user/customization.md#kubernetes-customization-unvalidated
        let mut cmd = cmd_launch_installer(&o);
        cmd.args(&["create", "manifests", "--dir"]);
        cmd.arg(&clusterdir);
        println!("Running create manifests");
        run_installer(&mut cmd)?;

        let openshiftdir = clusterdir.join("openshift");
        if !openshiftdir.exists() {
            std::fs::create_dir(&openshiftdir)?;
        }
        if let Some(ref manifests) = o.manifests {
            let mut copied: Vec<String> = Vec::new();
            for f in std::fs::read_dir(manifests).context("Failed to read manifests directory")? {
                let f = f?;
                if !f.file_type()?.is_file() {
                    continue;
                }
                let name = f.file_name();
                let name = match name.to_str() {
                    Some(s) => s,
                    None => continue,
                };
                let dest = Path::new(&openshiftdir).join(name);
                std::fs::copy(&f.path(), dest).context("Failed to copy manifest")?;
                copied.push(name.to_string());
            }
            if copied.is_empty() {
                bail!(
                    "No regular files found in additional manifests directory: {}",
                    manifests
                );
            }
            println!("Copied custom manifests:");
            for f in copied {
                println!("  {}", f);
            }
        }
        match o.size.as_ref() {
            Some(ClusterSize::Single) => {
                for (i, manifest) in SINGLE_MASTER_CONFIGS.iter().enumerate() {
                    std::fs::write(
                        openshiftdir.join(format!("singlemaster{}.yaml", i)),
                        manifest,
                    )?;
                }
            }
            _ => {}
        }
        match launched_config.config.platform {
            InstallConfigPlatform::Libvirt(_) => {
                for role in &["master", "worker"] {
                    let c: serde_json::Value = serde_json::from_str(config::AUTOLOGIN_CONFIG)?;
                    let v = config::machineconfig_from_ign_for_role(&c, "autologin", role);
                    let p = openshiftdir.join(format!("autologin-{}.json", role));
                    let mut f = std::io::BufWriter::new(std::fs::File::create(&p)?);
                    serde_json::to_writer_pretty(&mut f, &v)?;
                }
            }
            _ => {}
        };
    }

    let mut cmd = cmd_launch_installer(&o);
    cmd.args(&["create", "cluster", "--dir"]);
    cmd.arg(&clusterdir);
    if o.install_run_opts.log_debug {
        cmd.arg(format!("--log-level=debug"));
    }
    println!("Executing `openshift-install create cluster`");
    match run_installer(&mut cmd) {
        Ok(_) => Ok(()),
        Err(e) => {
            match (|| -> Result<()> {
                let p = clusterdir.join(FAILED_STAMP_PATH);
                let mut f = std::io::BufWriter::new(std::fs::File::create(&p)?);
                let e = e.to_string();
                f.write(e.as_bytes())?;
                f.flush()?;
                Ok(())
            })() {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("Writing {}: {}", FAILED_STAMP_PATH, e);
                }
            };
            Err(e)
        }
    }
}

/// Ensure base configdir, and get the path to a cluster
fn get_clusterdir(name: &str) -> Result<Box<std::path::Path>> {
    fs::create_dir_all(APPDIRS.config_dir())?;
    let clusterdir = APPDIRS.config_dir().join(name);
    if !clusterdir.exists() {
        bail!("No such cluster: {}", name);
    }
    Ok(clusterdir.into_boxed_path())
}

/// Destroy a cluster
fn destroy(name: &str, force: bool, debug: bool) -> Result<()> {
    let clusterdir = get_clusterdir(name)?;
    let launch_opts = get_launched_config(&clusterdir)?;
    let mut cmd = if let Some(ref launch_opts) = launch_opts {
        cmd_installer(
            launch_opts.installer_version.as_ref().map(|s| s.as_str()),
            launch_opts.libvirt_auto_size,
            None,
        )
    } else {
        eprintln!(
            "Warning: clusterdir {} missing launch opts file {}",
            name, LAUNCHED_CONFIG_PATH
        );
        cmd_installer(None, false, None)
    };
    let has_metadata = clusterdir.join(METADATA_PATH).exists();
    let failed = clusterdir.join(FAILED_STAMP_PATH).exists();
    if has_metadata {
        cmd.args(&["destroy", "cluster", "--dir"]);
        if debug {
            cmd.arg(format!("--log-level=debug"));
        }
        cmd.arg(&*clusterdir);
        println!("Executing `openshift-install destroy cluster`");
        let status = cmd
            .status()
            .map_err(|e| anyhow::anyhow!("Executing openshift-install").context(e))?;
        if !status.success() {
            if !force {
                bail!("openshift-install failed")
            } else {
                eprintln!("Warning: destroy cluster failed, continuing anyways")
            }
        }
    } else if failed {
        println!("Cluster {} failed install, just removing directory", name);
    } else {
        eprintln!("Cluster {} is missing {} but also does not have a failure stamp {}; continuing anyways", name, METADATA_PATH, FAILED_STAMP_PATH);
    }

    fs::remove_dir_all(clusterdir)?;
    Ok(())
}

/// Primary entrypoint
fn main() -> Result<()> {
    match Opt::from_args() {
        Opt::GenConfig(o) => {
            generate_config(o)?;
        }
        Opt::ListConfigs => {
            let configs = get_configs()?;
            print_list("configs", &configs.as_slice());
        }
        Opt::List => {
            print_clusters()?;
        }
        Opt::Launch(o) => {
            launch(o)?;
        }
        Opt::Destroy {
            name,
            force,
            install_run_opts,
        } => {
            destroy(&name, force, install_run_opts.log_debug)?;
        }
        Opt::Kubeconfig { name } => {
            let clusterdir = get_clusterdir(&name)?;
            println!("{}", clusterdir.join(KUBECONFIG_PATH).to_str().unwrap());
        }
    }

    Ok(())
}
