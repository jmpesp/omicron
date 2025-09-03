use anyhow::Result;
use anyhow::Context;
use anyhow::bail;
use camino::Utf8PathBuf;
use dropshot::ConfigDropshot;
use dropshot::ConfigLogging;
use dropshot::ConfigLoggingIfExists;
use dropshot::ConfigLoggingLevel;
use illumos_utils::dladm::PhysicalLink;
use sled_agent_types::rack_init::BootstrapAddressDiscovery;
use sled_agent_types::rack_init::RackInitializeRequest;
use omicron_sled_agent::config::Config as SledConfig;
use omicron_sled_agent::config::SledMode;
use omicron_sled_agent::config::SidecarRevision;
use omicron_sled_agent::updates::ConfigUpdates;
use omicron_common::address::IpRange;
use omicron_common::address::Ipv4Range;
use omicron_common::address::Ipv6Range;
use omicron_common::api::internal::shared::RackNetworkConfig;
use omicron_common::api::internal::shared::PortSpeed;
use omicron_common::api::internal::shared::PortFec;
use omicron_common::api::internal::shared::PortConfigV2;
use omicron_common::api::internal::shared::SwitchLocation;
use omicron_common::api::internal::shared::AllowedSourceIps;
use omicron_common::api::internal::shared::UplinkAddressConfig;
use omicron_common::api::internal::shared::RouteConfig;
use omicron_common::zpool_name::ZpoolName;
use omicron_common::zpool_name::ZPOOL_EXTERNAL_PREFIX;
use omicron_common::zpool_name::ZPOOL_INTERNAL_PREFIX;
use std::process::Command;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::str::FromStr;
use std::path::PathBuf;
use sled_hardware::disk::UnparsedDisk;
use sled_hardware::DiskFirmware;
use sled_hardware_types::Baseboard;
use sled_hardware_types::underlay::mac_to_bootstrap_ip;
use sled_agent_types::rack_init::RecoverySiloConfig;
use omicron_common::api::external::UserId;
use omicron_passwords::NewPasswordHash;
use omicron_passwords::Hasher;
use omicron_passwords::Password;
use sp_sim::config::SpCommonConfig;
use sp_sim::config::SpComponentConfig;
use sp_sim::config::SimulatedSpsConfig;
use hex::FromHex;
use gateway_messages::DeviceCapabilities;
use gateway_messages::DevicePresence;
use std::net::SocketAddrV6;
use std::net::Ipv6Addr;
use omicron_common::disk::DiskVariant;
use sprockets_tls::keys::SprocketsConfig;
use sprockets_tls::keys::ResolveSetting;
use omicron_gateway::RetryConfig;
use uuid::Uuid;
use sp_sim::config::EreportConfig;
use sp_sim::config::EreportRestart;

fn main() -> Result<()> {
    let cmd = Command::new("hostname").output()?;
    let hostname_with_newline = String::from_utf8_lossy(&cmd.stdout);
    let hostname = hostname_with_newline.trim();

    //const MULTI_SWITCH_MODE: bool = true;
    const MULTI_SWITCH_MODE: bool = false;

    let sled_mode = if MULTI_SWITCH_MODE {
        match hostname {
            "dinnerbone" | "gravytrain" => SledMode::Gimlet,
            "kibblesnbits" | "frostypaws" => SledMode::Scrimlet,
            _ => panic!("unknown hostname {}", hostname),
        }
    } else {
        match hostname {
            "dinnerbone" | "gravytrain" | "kibblesnbits"  => SledMode::Gimlet,
            "frostypaws" => SledMode::Scrimlet,

            _ => panic!("unknown hostname {}", hostname),
        }
    };

    // meant to be run from omicron root
    assert!(std::path::Path::new("smf/").exists());
    assert!(std::path::Path::new("smf/sled-agent/").exists());

    // depends on active target!
    assert!(std::path::Path::new("smf/sled-agent/non-gimlet").exists());

    eprintln!("sled mode \"{:?}\" hostname \"{}\"", sled_mode, hostname);

    let user_password_hash = {
        let password = Password::new("testpostpleaseignore").unwrap();
        let mut hasher = Hasher::default();
        hasher.create_password(&password).unwrap().to_string().try_into().unwrap()
    };

    const BOOTSTRAP_INTERFACE_ID: u64 = 1;

    // config-rss.toml
    if matches!(sled_mode, SledMode::Scrimlet) && hostname == "frostypaws" {
        let mut bootstrap_discovery_addrs = BTreeSet::new();

        // if you change a sled's data link, change these mac addresses!
        // dinnerbone ixgbe0
        bootstrap_discovery_addrs.insert(mac_to_bootstrap_ip("00:1b:21:c1:ff:e0".parse().unwrap(), BOOTSTRAP_INTERFACE_ID));
        // kibblesnbits ixgbe0
        bootstrap_discovery_addrs.insert(mac_to_bootstrap_ip("00:1b:21:c1:fc:da".parse().unwrap(), BOOTSTRAP_INTERFACE_ID));
        // gravytrain ixgbe0
        bootstrap_discovery_addrs.insert(mac_to_bootstrap_ip("00:1b:21:c1:fd:24".parse().unwrap(), BOOTSTRAP_INTERFACE_ID));
        // frostypaws ixgbe3
        bootstrap_discovery_addrs.insert(mac_to_bootstrap_ip("80:61:5f:11:ab:31".parse().unwrap(), BOOTSTRAP_INTERFACE_ID));

        let rss_config = RackInitializeRequest {
            trust_quorum_peers: Some(vec![
                Baseboard::new_pc(String::from("dinnerbone"), String::from("i86pc")),
                Baseboard::new_pc(String::from("kibblesnbits"), String::from("i86pc")),
                Baseboard::new_pc(String::from("gravytrain"), String::from("i86pc")),
                Baseboard::new_pc(String::from("frostypaws"), String::from("i86pc")),
                // non-existent: will cause `Fsm error= RackInitTimeout { unacked_peers: {Pc { identifier: "meowmix", model: "i86pc" }} }`
                // Baseboard::new_pc(String::from("meowmix"), String::from("i86pc")),
            ]),

            bootstrap_discovery: BootstrapAddressDiscovery::OnlyThese {
                 addrs: bootstrap_discovery_addrs,
            },

            /*gateway: Some(Gateway {
                // br0
                address: Some("10.1.0.50".parse()?),
                mac: "02:43:9b:55:ed:7c".parse()?,
            }),*/

            ntp_servers: vec![
                //String::from("ntp.ubuntu.com"),

                // fancyfeast
                String::from("10.0.0.1"),
            ],

            dns_servers: vec![
                "8.8.8.8".parse().unwrap(),
            ],

            internal_services_ip_pool_ranges: vec![
                IpRange::V4(Ipv4Range {
                    first: "10.1.0.10".parse().unwrap(),
                    last: "10.1.0.20".parse().unwrap(),
                }),

                // XXX test this later
                //IpRange::V6(Ipv6Range {
                //    first: "fd02:1122:3344:0101::3".parse().unwrap(),
                //    last: "fd02:1122:3344:0101::FF".parse().unwrap(),
                //}),
            ],

            external_dns_zone_name: "oxide.test".into(),

            external_dns_ips: vec![
                "10.1.0.10".parse().unwrap(),
            ],

            external_certificates: vec![],

            recovery_silo: RecoverySiloConfig {
                silo_name: "recovery".parse().unwrap(),
                user_name: UserId::try_from(String::from("admin")).unwrap(),
                user_password_hash,
            },

            // no opte config if this is None!
            // rack_network_config: None,

            rack_network_config: RackNetworkConfig {
                rack_subnet: "fd00:1122:3344:0100::/64".parse().unwrap(),

                // pool for switch ports
                infra_ip_first: "192.168.1.100".parse().unwrap(),
                infra_ip_last: "192.168.1.150".parse().unwrap(),

                ports: vec![
                    PortConfigV2 {
                        routes: vec![RouteConfig {
                            destination: "0.0.0.0/0".parse().unwrap(),
                            // fancyfeast interface connected to 10G network
                            nexthop: "192.168.1.1".parse().unwrap(),
                            vlan_id: None,
                            rib_priority: None,
                        }],

                        addresses: vec![UplinkAddressConfig {
                            // address from the infra ip pool to assign to the qsfp port
                            address: "192.168.1.100/24".parse().unwrap(),
                            vlan_id: None,
                        }],

                        // the frostypaws uplink should have 192.168.1.1
                        switch: SwitchLocation::Switch0,

                        // the name of the qsfp interface to use for reaching the
                        // default gateway (generally qsfp0)
                        port: String::from("qsfp0"),
                        uplink_port_speed: PortSpeed::Speed10G,
                        uplink_port_fec: None,
                        bgp_peers: vec![],
                        autoneg: false,
                        lldp: None,
                        tx_eq: None,
                    },

                    // XXX switch 1?
                ],

                bgp: vec![],

                bfd: vec![],
            },

            allowed_source_ips: AllowedSourceIps::Any,
        };

        // XXX would like:
        // parse_phc_hash(rss_config.recovery_silo.user_password_hash).unwrap();

        std::fs::write(
            "smf/sled-agent/non-gimlet/config-rss.toml",
            toml::to_string(&rss_config)?.as_bytes(),
        )?;
    } else {
        // If not frostypaws, do not run rss
        if std::path::Path::new("smf/sled-agent/non-gimlet/config-rss.toml")
            .exists()
        {
            std::fs::remove_file(
                "smf/sled-agent/non-gimlet/config-rss.toml",
            )?;
        }
    }

    // config.toml
    let sled_config = SledConfig {
        dropshot: ConfigDropshot {
            // bind address is ignored,

            default_request_body_max_bytes: 2147483648,

            ..ConfigDropshot::default()
        },

        log: ConfigLogging::File {
            level: ConfigLoggingLevel::Info,
            path: "/dev/stdout".into(),
            if_exists: ConfigLoggingIfExists::Append,
        },

        sled_mode: sled_mode.clone(),

        sidecar_revision: SidecarRevision::Physical(String::from("rev_a")),

        // prod gimlets set 80% of their memory as a VMM reservoir, so match
        // that here. When propolis is compiled with the "omicron-build"
        // feature it will return 500 if there isn't enough reservoir present to
        // launch your instance.
        //
        // however, this seemed to panic some of my desktops. so go a little
        // lower.
        vmm_reservoir_percentage: Some(60.0),
        vmm_reservoir_size_mb: None,

        // If this is set, then the sled agent will try to configure swap for
        // us. I've already done this so it's not required.
        swap_device_size_gb: None,

        vlan: None,

        vdevs: Some({
            // M2
            glob::glob("/home/james/*.vdev")?
                .flatten()
                .map(|x| x.to_string_lossy().to_string().into())
                .collect()
        }),

        nongimlet_observed_disks: Some({
            // Eat the output of nvmeadm to map device instance to (model,
            // serial).

            let cmd = Command::new("pfexec")
                .arg("nvmeadm")
                .arg("list")
                .arg("-p")
                .arg("-o")
                .arg("model,serial,instance")
                .output()?;

            let text = String::from_utf8_lossy(&cmd.stdout);

            let mut instance_map: HashMap<&str, (&str, &str)> = HashMap::new();

            for line in text.split("\n") {
                if line.len() == 0 {
                    continue;
                }

                let parts: Vec<&str> = line.split(':').collect();
                let model = parts[0];
                let serial = parts[1];
                let instance = parts[2];

                instance_map.insert(instance, (model, serial));
            }

            // Pass _all_ nvme disks as U2: walk all "nvme" nodes, then find the
            // underlying "blkdev", then use that devfs path.

            let mut slot = 0;
            let mut unparsed_disks = vec![];

            {
                let mut di = devinfo::DevInfo::new()?;

                let mut w = di.walk_driver("nvme");
                while let Some(n) = w.next().transpose()? {
                    let instance = format!(
                        "{}{}",
                        n.driver_name().unwrap(),
                        n.instance().unwrap(),
                    );

                    let (model, serial) = instance_map.get(instance.as_str()).unwrap();
                    let devfs_path = format!("/devices{}", n.devfs_path()?);

                    println!(
                        "> found {} / {} / {} / {}",
                        instance,
                        model,
                        serial,
                        devfs_path,
                    );

                    let mut bdi = devinfo::DevInfo::new_path(n.devfs_path()?)?;
                    let mut bw = bdi.walk_driver("blkdev");
                    let mut times = 0;

                    while let Some(bn) = bw.next().transpose()? {
                        if times != 0 {
                            bail!("multiple blkdev?!");
                        }

                        let devfs_path = format!("/devices{}", bn.devfs_path()?);

                        println!(
                            ">> using {}{}: {}",
                            bn.driver_name().unwrap(),
                            bn.instance().unwrap(),
                            devfs_path,
                        );

                        unparsed_disks.push(UnparsedDisk::new(
                            Utf8PathBuf::from(devfs_path),
                            None, // XXX Some(Utf8PathBuf::from(disk)) ? or /dev/dsk/{disk} ?
                            slot,
                            DiskVariant::U2,
                            omicron_common::disk::DiskIdentity {
                                // sled-hardware/src/illumos/mod.rs, find_properties
                                vendor: String::from("Synthetic"),
                                serial: String::from(*serial),
                                model: String::from(*model),
                            },
                            false, // is_boot_disk
                            DiskFirmware::new(
                                /*active_slot*/ 1, // NVMe spec has slots 1-7
                                /*next_active_slot*/ None,
                                /*slot1_read_only*/ true,
                                /*number of slots*/1,
                                /*slots*/ vec![Some(String::from("firmware"))],
                            ),
                        ));

                        slot += 1;
                        times += 1;
                    }
                }
            }

            unparsed_disks
        }),

        skip_timesync: Some(false),

        data_link: Some(PhysicalLink(String::from(match hostname {
            "dinnerbone" => "ixgbe0",
            "kibblesnbits" => "ixgbe0",
            "gravytrain" => "ixgbe0",
            "frostypaws" => "ixgbe3",
            _ => panic!("unknown hostname {}", hostname),
        }))),

        data_links: if MULTI_SWITCH_MODE {
            match hostname {
                "dinnerbone" => ["ixgbe0".to_string(), "igb0".to_string()],

                // kibblesnbits has e1000g3 and e1000g5 plugged into each other,
                // choose e1000g5 as the opte underlay and e1000g3 as the maghemite
                // switch zone one
                "kibblesnbits" => ["ixgbe0".to_string(), "e1000g5".to_string()],

                "gravytrain" => ["ixgbe0".to_string(), "igb0".to_string()],

                // frostypaws has ixgbe3 and ixgbe4 plugged into each other, choose
                // ixgbe3 as the opte underlay and ixgbe4 as the maghemite switch
                // zone one
                "frostypaws" => ["ixgbe3".to_string(), "igb0".to_string()],

                _ => panic!("unknown hostname {}", hostname),
            }
        } else {
            match hostname {
                "dinnerbone" => ["ixgbe0".to_string(), "net1".to_string()],
                "kibblesnbits" => ["ixgbe0".to_string(), "net1".to_string()],
                "gravytrain" => ["ixgbe0".to_string(), "net1".to_string()],
                "frostypaws" => ["ixgbe3".to_string(), "net1".to_string()],
                _ => panic!("unknown hostname {}", hostname),
            }
        },

        updates: ConfigUpdates::default(),

        switch_zone_maghemite_links: match hostname {
            "dinnerbone" => vec![],
            "kibblesnbits" => if MULTI_SWITCH_MODE {
                // kibblesnbits has e1000g3 and e1000g5 plugged into each other,
                // choose e1000g5 as the opte and e1000g3 as the maghemite switch zone
                // one.
                vec![
                    PhysicalLink("e1000g4".into()),
                    PhysicalLink("e1000g0".into()),
                    PhysicalLink("e1000g1".into()),
                    PhysicalLink("e1000g2".into()),
                    PhysicalLink("e1000g3".into()),
                ]
            } else {
                vec![]
            },
            "gravytrain" => vec![],
            // frostypaws has ixgbe3 and ixgbe4 plugged into each other, choose
            // ixgbe3 as the opte and ixgbe4 as the maghemite switch zone one
            "frostypaws" => vec![
                PhysicalLink("ixgbe4".into()),
                PhysicalLink("ixgbe0".into()),
                PhysicalLink("ixgbe1".into()),
                PhysicalLink("ixgbe2".into()),
                PhysicalLink("ixgbe5".into()),
            ],
            _ => panic!("unknown hostname {}", hostname),
        },

        sprockets: SprocketsConfig {
            resolve: ResolveSetting::Local {
                priv_key: Utf8PathBuf::from(
                    format!("/home/james/omicron/sprockets_tls/{hostname}.key.pem")
                ),

                cert_chain: Utf8PathBuf::from(
                    format!("/home/james/omicron/sprockets_tls/{hostname}.cert.pem")
                ),
            },

            roots: vec![
                Utf8PathBuf::from("/home/james/omicron/sprockets_tls/canada_region_ca.cert.pem"),
            ],
        },

        control_plane_memory_earmark_mb: Some(6144),
    };

    std::fs::write(
        "smf/sled-agent/non-gimlet/config.toml",
        toml::to_string(&sled_config)?.as_bytes(),
    )?;

    // Simulated SP in the switch zone for the sled
    let sp_sim_config = sp_sim::config::Config {
        simulated_sps: SimulatedSpsConfig {
            sidecar: if matches!(sled_mode, SledMode::Scrimlet) {
                vec![
                    sp_sim::config::SidecarConfig { common: SpCommonConfig {
                        network_config: Some([
                            // should match mgs's SwitchPortDescription
                            sp_sim::config::NetworkConfig::Simulated {
                                bind_addr: "[::]:33300".parse().unwrap(),
                            },

                            sp_sim::config::NetworkConfig::Simulated {
                                bind_addr: "[::]:33301".parse().unwrap(),
                            }
                        ]),

                        // XXX does this have to match mgs location map?
                        serial_number: match hostname {
                            "frostypaws" => String::from("switch0"),
                            "kibblesnbits" => String::from("switch1"),

                            _ => panic!("unknown hostname {}", hostname),
                        },

                        // can ignore
                        manufacturing_root_cert_seed:
                            <[u8; 32]>::from_hex("01de01de01de01de01de01de01de01de01de01de01de01de01de01de01de01de").unwrap(),

                        // can ignore
                        device_id_cert_seed:
                            <[u8; 32]>::from_hex("01de000000000000000000000000000000000000000000000000000000000001").unwrap(),

                        components: vec![],

                        no_stage0_caboose: true,

                        old_rot_state: false,

                        ereport_config: EreportConfig {
                            restart: EreportRestart {
                                restart_id: Uuid::new_v4(),
                                metadata: toml::map::Map::default(),
                            },
                            ereports: vec![],
                        },

                        ereport_network_config: None,
                    }}
                ]
            } else {
                vec![]
            },

            gimlet: vec![],
        },

        log: ConfigLogging::File {
            level: ConfigLoggingLevel::Info,
            path: "/dev/stdout".into(),
            if_exists: ConfigLoggingIfExists::Append,
        },
    };

    std::fs::write(
        "smf/sp-sim/config.toml",
        toml::to_string(&sp_sim_config)?.as_bytes(),
    )?;

    const MGS_TO_SP_MULTICAST_ADDR: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0x1de, 2);
    pub const SP_PORT: u16 = 11111;

    let multicast_addr = SocketAddrV6::new(MGS_TO_SP_MULTICAST_ADDR, SP_PORT, 0, 0);

    // Simulated SP in the global zone for the sled
    let sp_sim_config = sp_sim::config::Config {
        simulated_sps: SimulatedSpsConfig {
            sidecar: vec![],

            gimlet: vec![
                sp_sim::config::GimletConfig { common: SpCommonConfig {
                    // use first entry for switch 0, second for switch 1 MGS
                    // will grab based on bootstrap address, listen on same port
                    // for all
                    network_config: Some([
                        sp_sim::config::NetworkConfig::Real {
                            bind_addr: "[::]:11111".parse().unwrap(), // XXX has to be SP port
                            multicast_addr: MGS_TO_SP_MULTICAST_ADDR,
                            multicast_interface: match hostname {
                                "dinnerbone" |
                                "kibblesnbits" |
                                "gravytrain" => String::from("ixgbe0"),

                                "frostypaws" => String::from("ixgbe3"),

                                _ => panic!("hostname not recognized!"),
                            },
                        },

                        sp_sim::config::NetworkConfig::Real {
                            // XXX would like same port, use link local addr?
                            bind_addr: "[::1]:11112".parse().unwrap(),
                            multicast_addr: MGS_TO_SP_MULTICAST_ADDR,
                            // XXX MULTI_SWITCH_MODE
                            multicast_interface: String::from("net1"),
                        },
                    ]),

                    serial_number: hostname.to_string(),

                    // can ignore
                    manufacturing_root_cert_seed:
                        <[u8; 32]>::from_hex("01de01de01de01de01de01de01de01de01de01de01de01de01de01de01de01de").unwrap(),

                    // can ignore
                    device_id_cert_seed:
                        <[u8; 32]>::from_hex("01de000000000000000000000000000000000000000000000000000000000001").unwrap(),

                    components: vec![
                        /*
                        // cpu
                        SpComponentConfig {
                            id: String::from("sp3-host-cpu"),
                            device: String::from("sp3-host-cpu"),
                            description: String::from("FAKE host cpu"),
                            capabilities: DeviceCapabilities::from_bits(0).unwrap(), // XXX no HAS_SERIAL_CONSOLE
                            presence: DevicePresence::Present,
                            serial_console: None, // Some("[::1]:33312"), // XXX no HAS_SERIAL_CONSOLE
                        },
                        */

                        // TODO: U.2 components:
                        // Present U.2 ABCD mux (pca9545)
                        // Present U.2 EFGH mux (pca9545)
                        // Present U.2 IJ/FRUID mux (pca9545)
                        // Present U.2 Sharkfin A VPD (at24csw080)
                        // Present U.2 Sharkfin A hot swap controller (max5970)
                        // Present U.2 A NVMe Basic Management Command (nvme_bmc)
                        // Present U.2 Sharkfin B VPD (at24csw080)
                        // Present U.2 Sharkfin B hot swap controller (max5970)
                        // Present U.2 B NVMe Basic Management Control (nvme_bmc)
                        // Present U.2 Sharkfin C VPD (at24csw080)
                        // Present U.2 Sharkfin C hot swap controller (max5970)
                        // Present U.2 C NVMe Basic Management Control (nvme_bmc)
                        // Present U.2 Sharkfin D VPD (at24csw080)
                        // Present U.2 Sharkfin D hot swap controller (max5970)
                        // Present U.2 D NVMe Basic Management Control (nvme_bmc)
                        // Present U.2 Sharkfin E VPD (at24csw080)
                        // Present U.2 Sharkfin E hot swap controller (max5970)
                        // Present U.2 E NVMe Basic Management Control (nvme_bmc)
                        // Present U.2 Sharkfin F VPD (at24csw080)
                        // Present U.2 Sharkfin F hot swap controller (max5970)
                        // Present U.2 F NVMe Basic Management Control (nvme_bmc)
                        // Present U.2 Sharkfin G VPD (at24csw080)
                        // Present U.2 Sharkfin G hot swap controller (max5970)
                        // Present U.2 G NVMe Basic Management Control (nvme_bmc)
                        // Present U.2 Sharkfin H VPD (at24csw080)
                        // Present U.2 Sharkfin H hot swap controller (max5970)
                        // Present U.2 H NVMe Basic Management Control (nvme_bmc)
                        // Present U.2 Sharkfin I VPD (at24csw080)
                        // Present U.2 Sharkfin I hot swap controller (max5970)
                        // Present U.2 I NVMe Basic Management Control (nvme_bmc)
                        // Present U.2 Sharkfin J VPD (at24csw080)
                        // Present U.2 Sharkfin J hot swap controller (max5970)
                        // Present U.2 J NVMe Basic Management Control (nvme_bmc)
                    ],

                    no_stage0_caboose: true,

                    old_rot_state: false,

                    ereport_config: EreportConfig {
                        restart: EreportRestart {
                            restart_id: Uuid::new_v4(),
                            metadata: toml::map::Map::default(),
                        },
                        ereports: vec![],
                    },

                    ereport_network_config: None,
                }},
            ],
        },

        log: ConfigLogging::File {
            level: ConfigLoggingLevel::Info,
            path: "/dev/stdout".into(),
            if_exists: ConfigLoggingIfExists::Append,
        },
    };

    std::fs::write(
        "smf/sp-sim/config-global.toml",
        toml::to_string(&sp_sim_config)?.as_bytes(),
    )?;

    if matches!(sled_mode, SledMode::Scrimlet) {
        // run simulated MGS in switch zone
        let port = {
            let mut port = vec![
                // sidecar 0 - frostypaws
                omicron_gateway::SwitchPortDescription {
                    /*
                    config: omicron_gateway::SwitchPortConfig::SwitchZoneInterface {
                        interface: String::from("lo0"),
                    },
                    */
                    config: omicron_gateway::SwitchPortConfig::Simulated {
                        fake_interface: String::from("sidecar0"), // has to match local ignition interface?
                        addr: "[::1]:33300".parse().unwrap(),
                        ereport_addr: "[::1]:43300".parse().unwrap(),
                    },

                    ignition_target: 0,

                    location: vec![
                        (
                            String::from("switch0"),
                            omicron_gateway::SpIdentifier {
                                typ: omicron_gateway::SpType::Switch,
                                slot: 0,
                            },
                        ),
                        (
                            String::from("switch1"),
                            omicron_gateway::SpIdentifier {
                                typ: omicron_gateway::SpType::Switch,
                                slot: 1,
                            },
                        ),
                    ].into_iter().collect(),
                },
            ];

            if MULTI_SWITCH_MODE {
                todo!("MULTI_SWITCH_MODE");

                // XXX omicron_gateway::SwitchPortConfig::Simulated connecting
                // to other switch zone? or, real, and listening for multicast?
            }

            struct PortConfig {
                switch_zone_interface: String,
                sled_number: usize,
                ignition_target: u8,
            }

            let config = vec![
                // dinnerbone interface in switch zone = sled 0
                PortConfig {
                    switch_zone_interface: String::from("ixgbe1"),
                    sled_number: 0,
                    ignition_target: 3,
                },
                // kibblesnbits interface in switch zone = sled 1
                PortConfig {
                    switch_zone_interface: String::from("ixgbe0"),
                    sled_number: 1,
                    ignition_target: 4,
                },
                // gravytrain interface in switch zone = sled 2
                PortConfig {
                    switch_zone_interface: String::from("ixgbe2"),
                    sled_number: 2,
                    ignition_target: 5,
                },
                // frostypaws interface in switch zone = sled 3
                PortConfig {
                    switch_zone_interface: String::from("ixgbe4"),
                    sled_number: 3,
                    ignition_target: 6,
                },
            ];

            port.extend(
                config
                    .into_iter()
                    .map(|config| omicron_gateway::SwitchPortDescription {
                        config: omicron_gateway::SwitchPortConfig::SwitchZoneInterface {
                            interface: config.switch_zone_interface,
                        },

                        ignition_target: config.ignition_target,

                        location: vec![
                            (
                                String::from("switch0"),
                                omicron_gateway::SpIdentifier {
                                    typ: omicron_gateway::SpType::Sled,
                                    slot: config.sled_number as u16,
                                },
                            ),
                            (
                                String::from("switch1"),
                                omicron_gateway::SpIdentifier {
                                    typ: omicron_gateway::SpType::Sled,
                                    slot: config.sled_number as u16,
                                },
                            ),
                        ].into_iter().collect(),
                    })
                    .collect::<Vec<_>>(),
            );

            port
        };

        let mgs_sim_config = omicron_gateway::Config {
            host_phase2_recovery_image_cache_max_images: 1,

            dropshot: omicron_gateway::PartialDropshotConfig {
                default_request_body_max_bytes: 536870912,
            },

            switch: omicron_gateway::SwitchConfig {
                udp_listen_port: 12225, // gateway_sp_comms::MGS_PORT

                // name of interface, not port
                local_ignition_controller_interface: match hostname {
                    "frostypaws" => String::from("sidecar0"),
                    "kibblesnbits" => String::from("sidecar1"), // XXX TODO

                    _ => panic!("unknown hostname {}", hostname),
                },

                rpc_retry_config: RetryConfig {
                    per_attempt_timeout_millis: 2000,
                    // how many attempts to reset before giving up
                    max_attempts_reset: 30,
                    // how many attempts other than to reset before giving up
                    max_attempts_general: 5,
                },

                location: omicron_gateway::LocationConfig {
                    // possible locations where MGS could be running
                    names: vec![
                        String::from("switch0"), // XXX frostypaws
                        String::from("switch1"), // XXX kibblesnbits
                    ],

                    // - the list of switch ports to contact to determine
                    //   location
                    // - each port is a subset of names vec above
                    determination: vec![
                        omicron_gateway::LocationDeterminationConfig {
                            interfaces: match hostname {
                                "frostypaws" => vec![
                                    String::from("ixgbe0"),
                                    String::from("ixgbe1"),
                                    String::from("ixgbe2"),
                                    String::from("ixgbe4"),
                                    String::from("sidecar0"),
                                    // XXX sidecar1?
                                ],

                                "kibblesnbits" => {
                                    todo!("MULTI_SWITCH_MODE");
                                },

                                _ => panic!("switch hostname not recognized"),
                            },
                            sp_port_1: vec![String::from("switch0")],
                            sp_port_2: vec![String::from("switch1")],
                        }
                    ],
                },

                port,

                ereport_udp_listen_port: 1225, // bogus
            },

            log: ConfigLogging::File {
                level: ConfigLoggingLevel::Info,
                path: "/dev/stdout".into(),
                if_exists: ConfigLoggingIfExists::Append,
            },

            metrics: None, // MetricsConfig
        };

        std::fs::write(
            "smf/mgs-sim/config.toml",
            toml::to_string(&mgs_sim_config)?.as_bytes(),
        )?;
    }

    Ok(())
}
