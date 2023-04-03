use serde_json::json;

/// Enable automatic login on consoles, taken from
/// https://github.com/coreos/coreos-assembler/blob/5e391c8646be63b4cf29554c0a033f9195db4b90/mantle/platform/conf/conf.go#L1255
pub(crate) static AUTOLOGIN_CONFIG: &str = r###"
{
  "ignition": {
    "version": "3.0.0"
  },
  "systemd": {
    "units": [
      {
        "dropins": [
          {
            "contents": "[Service]\n\tExecStart=\n\tExecStart=-/sbin/agetty --autologin core -o '-p -f core' --noclear %I $TERM\n\t",
            "name": "10-autologin.conf"
          }
        ],
        "name": "getty@.service"
      },
      {
        "dropins": [
          {
            "contents": "[Service]\n\tExecStart=\n\tExecStart=-/sbin/agetty --autologin core -o '-p -f core' --keep-baud 115200,38400,9600 %I $TERM\n\t",
            "name": "10-autologin.conf"
          }
        ],
        "name": "serial-getty@.service"
      }
    ]
  }
}
"###;

/// Given an Ignition configuration, generate MachineConfig
/// to use.
pub(crate) fn machineconfig_from_ign_for_role(
    ign: &serde_json::Value,
    name: &str,
    role: &str,
) -> serde_json::Value {
    json!({
        "apiVersion": "machineconfiguration.openshift.io/v1",
        "kind": "MachineConfig",
        "metadata": {
            "labels": {
                "machineconfiguration.openshift.io/role": role,
            },
            "name": format!("42-{}-{}", name, role),
        },
        "spec": {
          "config": ign,
        }
    })
}
