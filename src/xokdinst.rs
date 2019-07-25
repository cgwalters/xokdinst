use std::{fs, io};
use std::io::prelude::*;
use std::borrow::Cow;
use std::collections::HashMap;
use structopt::StructOpt;
// https://github.com/clap-rs/clap/pull/1397
#[macro_use]
extern crate clap;
use directories;
use failure::{Fallible, bail, format_err};
use lazy_static::lazy_static;
use serde_derive::{Serialize, Deserialize};
use tabwriter::TabWriter;

lazy_static! {
    static ref APPDIRS : directories::ProjectDirs = directories::ProjectDirs::from("org", "openshift", "xokdinst").expect("creating appdirs");
}

static LAUNCHED_CONFIG_PATH : &str = "xokdinst-launched-config.yaml";
static FAILED_STAMP_PATH : &str = "xokdinst-failed";
static KUBECONFIG_PATH : &str = "auth/kubeconfig";
static METADATA_PATH : &str = "metadata.json";
/// Relative path in home to credentials used to authenticate to registries
/// The podman stack uses a different path by default but will honor this
/// one if it exists.
static DOCKERCFG_PATH : &str = ".docker/config.json";

/// Holds extra keys from a map we didn't explicitly parse
type SerdeYamlMap = HashMap<String, serde_yaml::Value>;

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
enum InstallConfigPlatform {
    Libvirt(SerdeYamlMap),
    AWS(SerdeYamlMap),
}

#[derive(Serialize, Deserialize, Clone)]
#[allow(dead_code)]
struct InstallConfigMachines {
    name: String,
    replicas: u32,
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
    extra: SerdeYamlMap
}

/// State used to launch a cluster
#[derive(Serialize, Deserialize)]
struct LaunchedConfig {
    config: InstallConfig,
    // Subset of LaunchOpts
    installer_version: Option<String>,
    release_image: Option<String>,
    boot_image: Option<String>,
}

arg_enum! {
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    enum Platform {
        Libvirt,
        AWS,
    }
}

impl InstallConfigPlatform {
    fn to_platform(&self) -> Platform {
        match self {
            InstallConfigPlatform::Libvirt(_) => Platform::Libvirt,
            InstallConfigPlatform::AWS(_) => Platform::AWS,
        }
    }
}

#[derive(Debug, StructOpt)]
struct LaunchOpts {
    /// Name of the cluster to launch
    name: String,

    #[structopt(short = "D")]
    /// Delete an existing cluster, if one exists
    destroy: bool,

    #[structopt(short = "V", long = "instversion")]
    /// Use a versioned installer binary
    installer_version: Option<String>,

    #[structopt(short = "p", raw(possible_values = "&Platform::variants()", case_insensitive = "true"))]
    platform: Option<Platform>,

    #[structopt(short = "c", long = "config")]
    /// The name of the base configuration (overrides platform)
    config: Option<String>,

    /// Override the release image (for development/testing)
    #[structopt(short = "I", long = "release-image")]
    release_image: Option<String>,

    /// Override the RHCOS bootimage image (for development/testing)
    #[structopt(short = "O", long = "boot-image")]
    boot_image: Option<String>,

    /// Inject objects during installation (e.g. MachineConfig)
    /// See https://github.com/openshift/installer/blob/master/docs/user/customization.md#kubernetes-customization-unvalidated
    #[structopt(long = "manifests")]
    manifests: Option<String>,
}

#[derive(Debug, StructOpt)]
struct GenConfigOpts {
    /// Name for this configuration; if not specified, will be named config-<platform>.yaml
    name: Option<String>,
    /// Overwrite an existing default configuration
    overwrite: bool,
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
        force: bool
    },
}

fn cmd_installer(version: Option<&str>) -> std::process::Command {
    let path = if let Some(version) = version {
        Cow::Owned(format!("openshift-install-{}", version))
    } else {
        Cow::Borrowed("openshift-install")
    };
    let mut cmd = std::process::Command::new(path.as_ref());
    // https://github.com/openshift/installer/pull/1890
    cmd.env("OPENSHIFT_INSTALL_INVOKER", "xokdinst");
    // Override the libvirt defaults
    // TODO(walters) compute these from what's available on the host
    // https://github.com/openshift/installer/pull/785
    cmd.env("TF_VAR_libvirt_master_memory", "8192")
        .env("TF_VAR_libvirt_master_vcpu", "4");
    cmd
}

fn run_installer(cmd: &mut std::process::Command) -> Fallible<()> {
    let status = cmd.status().map_err(|e| format_err!("Executing openshift-install").context(e))?;
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
fn generate_config(o: GenConfigOpts) -> Fallible<String> {
    if let Some(name) = o.name.as_ref() {
        let path = APPDIRS.config_dir().join(&name);
        if !o.overwrite && path.exists() {
            bail!("Configuration '{}' already exists and overwrite not specified", name);
        }
    }

    let tmpd = tempfile::Builder::new().prefix("xokdinst").tempdir()?;
    println!("Executing `openshift-install create install-config`");
    let mut cmd = cmd_installer(None);
    cmd.args(&["create", "install-config", "--dir"]);
    cmd.arg(tmpd.path());
    run_installer(&mut cmd)?;

    let tmp_config_path = tmpd.path().join("install-config.yaml");
    let mut parsed_config : InstallConfig = serde_yaml::from_reader(io::BufReader::new(fs::File::open(tmp_config_path)?))?;
    let platform = parsed_config.platform.to_platform();
    let name = get_config_name(&o.name, &platform);
    let path = APPDIRS.config_dir().join(&name);
    if !o.overwrite && path.exists() {
        bail!("Configuration '{}' already exists and overwrite not specified", name);
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
fn get_configs() -> Fallible<Vec<String>> {
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

fn get_launched_config<P>(clusterdir: P) -> Fallible<Option<LaunchedConfig>>
    where P: AsRef<std::path::Path>
{
    let clusterdir = clusterdir.as_ref();
    let path = clusterdir.join(LAUNCHED_CONFIG_PATH);
    let config : Option<LaunchedConfig> = if path.exists() {
        let mut r = io::BufReader::new(fs::File::open(path)?);
        Some(serde_yaml::from_reader(&mut r)?)
    } else {
        None
    };
    Ok(config)
}

/// Get all cluster names
fn get_clusters() -> Fallible<Vec<String>> {
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

fn print_clusters() -> Fallible<()> {
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
    let installer_version = o.installer_version.as_ref().map(|x|x.as_str());
    let mut cmd = cmd_installer(installer_version);
    if let Some(ref image) = o.release_image {
        cmd.env("OPENSHIFT_INSTALL_RELEASE_IMAGE_OVERRIDE", image);
    }
    if let Some(ref image) = o.boot_image {
        cmd.env("OPENSHIFT_INSTALL_OS_IMAGE_OVERRIDE", image);
    }
    cmd
}

/// 🚀
fn launch(o: LaunchOpts) -> Fallible<()> {
    fs::create_dir_all(APPDIRS.config_dir())?;
    let clusterdir = APPDIRS.config_dir().join(&o.name);
    if clusterdir.exists() {
        if !o.destroy {
            bail!("Cluster {} already exists, use destroy to remove it", o.name.as_str());
        }
        destroy(&o.name, true)?;
    }
    let configs = get_configs()?;
    let config_name = if o.config.is_none() && configs.len() == 0 {
        println!("No configurations found; generating one now");
        Cow::Owned(generate_config(GenConfigOpts {
            name: None,
            overwrite: false,
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
    let config_path : Option<_> = [&config_name, &full_name]
        .iter().map(|c| APPDIRS.config_dir().join(c.as_str())).filter(|c| c.exists()).next();
    let config_path = match config_path {
        Some(x) => x,
        None => bail!("No such configuration: {}", config_name),
    };

    let mut config : InstallConfig = serde_yaml::from_reader(io::BufReader::new(fs::File::open(config_path)?))?;
    config.metadata = Some(InstallConfigMetadata {
        name: o.name.to_string(),
        extra: None,
    });

    // If there's no pull secret, automatically use ~/.docker/config.json
    if config.pull_secret.is_none() {
        let dirs = match directories::BaseDirs::new() {
            Some(x) => x,
            None => bail!("No HOME found"),
        };
        let dockercfg_path = dirs.home_dir().join(DOCKERCFG_PATH);
        if !dockercfg_path.exists() {
            bail!("No pull secret in install config, and no {} found", DOCKERCFG_PATH);
        }
        let pull_secret = std::fs::read_to_string(dockercfg_path)?;
        config.pull_secret = Some(pull_secret);
    }

    fs::create_dir(&clusterdir)?;
    let mut w = io::BufWriter::new(fs::File::create(clusterdir.join(LAUNCHED_CONFIG_PATH))?);
    let launched_config = LaunchedConfig {
        config: config.clone(),
        installer_version: o.installer_version.clone(),
        release_image: o.release_image.clone(),
        boot_image: o.boot_image.clone(),
    };
    serde_yaml::to_writer(&mut w, &launched_config)?;
    w.flush()?;

    let mut w = io::BufWriter::new(fs::File::create(clusterdir.join("install-config.yaml"))?);
    serde_yaml::to_writer(&mut w, &config)?;
    w.flush()?;

    let mut cmd = cmd_launch_installer(&o);
    cmd.arg("version");
    run_installer(&mut cmd)?;


    if let Some(ref manifests) = o.manifests {
        // https://github.com/openshift/installer/blob/master/docs/user/customization.md#kubernetes-customization-unvalidated
        let mut cmd = cmd_launch_installer(&o);
        cmd.args(&["create", "manifests", "--dir"]);
        cmd.arg(&clusterdir);
        println!("Copying custom manifests: {}", manifests);
        let openshiftdir = clusterdir.join("openshift");
        let mut copy = std::process::Command::new("cp");
        copy.args(&["-rp", "--reflink=auto"])
            .arg(manifests)
            .arg(&openshiftdir);
        if !copy.status()?.success() {
            bail!("Failed to copy manifests")
        }
        run_installer(&mut cmd)?;
    }

    let mut cmd = cmd_launch_installer(&o);
    cmd.args(&["create", "cluster", "--dir"]);
    cmd.arg(&clusterdir);
    println!("Executing `openshift-install create cluster`");
    match run_installer(&mut cmd) {
        Ok(_) => Ok(()),
        Err(e) => {
            match (|| -> Fallible<()> {
                let p = clusterdir.join(FAILED_STAMP_PATH);
                let mut f = std::io::BufWriter::new(std::fs::File::create(&p)?);
                let e = e.to_string();
                f.write(e.as_bytes())?;
                f.flush()?;
                Ok(())
            })() {
                Ok(_) => {},
                Err(e) => {
                    eprintln!("Writing {}: {}", FAILED_STAMP_PATH, e);
                }
            };
            Err(e)
        }
    }
}

/// Ensure base configdir, and get the path to a cluster
fn get_clusterdir(name: &str) -> Fallible<Box<std::path::Path>> {
    fs::create_dir_all(APPDIRS.config_dir())?;
    let clusterdir = APPDIRS.config_dir().join(name);
    if !clusterdir.exists() {
        bail!("No such cluster: {}", name);
    }
    Ok(clusterdir.into_boxed_path())
}

/// Destroy a cluster
fn destroy(name: &str, force: bool) -> Fallible<()> {
    let clusterdir = get_clusterdir(name)?;
    let launch_opts = get_launched_config(&clusterdir)?;
    let mut cmd = if let Some(ref launch_opts) = launch_opts {
        cmd_installer(launch_opts.installer_version.as_ref().map(|s|s.as_str()))
    } else {
        eprintln!("Warning: clusterdir {} missing launch opts file {}", name, LAUNCHED_CONFIG_PATH);
        cmd_installer(None)
    };
    let has_metadata = clusterdir.join(METADATA_PATH).exists();
    let failed = clusterdir.join(FAILED_STAMP_PATH).exists();
    if has_metadata {
        cmd.args(&["destroy", "cluster", "--dir"]);
        cmd.arg(&*clusterdir);
        println!("Executing `openshift-install destroy cluster`");
        let status = cmd.status().map_err(|e| format_err!("Executing openshift-install").context(e))?;
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
fn main() -> Fallible<()> {
    match Opt::from_args() {
        Opt::GenConfig(o) => {
            generate_config(o)?;
        },
        Opt::ListConfigs => {
            let configs = get_configs()?;
            print_list("configs", &configs.as_slice());
        },
        Opt::List => {
            print_clusters()?;
        },
        Opt::Launch(o) => {
            launch(o)?;
        },
        Opt::Destroy { name, force } => {
            destroy(&name, force)?;
        },
        Opt::Kubeconfig { name } => {
            let clusterdir = get_clusterdir(&name)?;
            println!("{}", clusterdir.join(KUBECONFIG_PATH).to_str().unwrap());
        },
    }

    Ok(())
}
