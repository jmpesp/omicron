use anyhow::Result;
use anyhow::Context;
use dropshot::ConfigDropshot;
use dropshot::ConfigLogging;
use dropshot::ConfigLoggingIfExists;
use dropshot::ConfigLoggingLevel;
use illumos_utils::dladm::PhysicalLink;
use illumos_utils::zpool::ZpoolName;
use illumos_utils::zpool::ZPOOL_EXTERNAL_PREFIX;
use illumos_utils::zpool::ZPOOL_INTERNAL_PREFIX;
use omicron_sled_agent::bootstrap::params::BootstrapAddressDiscovery;
use omicron_sled_agent::config::Config as SledConfig;
use omicron_sled_agent::config::SledMode;
use omicron_sled_agent::config::SidecarRevision;
use omicron_sled_agent::rack_setup::config::SetupServiceConfig as RssConfig;
use omicron_sled_agent::updates::ConfigUpdates;
use omicron_common::address::IpRange;
use omicron_common::address::Ipv4Range;
use omicron_common::address::Ipv6Range;
use omicron_common::api::internal::shared::RackNetworkConfig;
use omicron_common::api::internal::shared::PortSpeed;
use omicron_common::api::internal::shared::PortFec;
use omicron_common::api::internal::shared::SwitchLocation;
use omicron_common::api::internal::shared::UplinkConfig;
use std::process::Command;
use std::collections::HashSet;
use std::collections::HashMap;
use std::str::FromStr;
use sled_hardware::Baseboard;
use sled_hardware::underlay::mac_to_bootstrap_ip;
use nexus_client::types::RecoverySiloConfig;
use nexus_client::types::UserId;
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

fn main() -> Result<()> {
    let cmd = Command::new("hostname").output()?;
    let hostname = String::from_utf8_lossy(&cmd.stdout);

    //const MULTI_SWITCH_MODE: bool = true;
    const MULTI_SWITCH_MODE: bool = false;

    let sled_mode = if MULTI_SWITCH_MODE {
        match hostname.trim() {
            "dinnerbone" | "gravytrain" => SledMode::Gimlet,
            "kibblesnbits" | "frostypaws" => SledMode::Scrimlet,
            _ => panic!("unknown hostname {}", hostname),
        }
    } else {
        match hostname.trim() {
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

    eprintln!("sled mode \"{:?}\" hostname \"{}\"", sled_mode, hostname.trim());

    let user_password_hash = {
        // echo -n 'testpostpleaseignore' | argon2 $(pwgen 32 1) -id -t 15 -k 98304 -l 32 -p 1
        let user_password_hash_str =
            "$argon2id$v=19$m=98304,t=15,p=1$dGVpTGFlcXVvb2Nob3RoYWhyZXdhMWVpRnVjMmNhdTA$zc81Dk4+UGJemtcK2NzC/e/7y962cKBm8nIYx5I894o";
        let user_password_hash = NewPasswordHash::try_from(user_password_hash_str.to_string()).unwrap();
        let hasher = Hasher::new(omicron_passwords::external_password_argon(), rand::thread_rng());
        assert!(hasher.verify_password(
            &Password::new("testpostpleaseignore").unwrap(),
            &user_password_hash.clone().into(),
        ).unwrap());
        user_password_hash
    };

    const BOOTSTRAP_INTERFACE_ID: u64 = 1;

    // config-rss.toml
    if matches!(sled_mode, SledMode::Scrimlet) && hostname.trim() == "frostypaws" {
        let mut bootstrap_discovery_addrs = HashSet::new();

        // if you change a sled's data link, change these mac addresses!
        // dinnerbone ixgbe0
        bootstrap_discovery_addrs.insert(mac_to_bootstrap_ip("00:1b:21:c1:ff:e0".parse().unwrap(), BOOTSTRAP_INTERFACE_ID));
        // kibblesnbits ixgbe0
        bootstrap_discovery_addrs.insert(mac_to_bootstrap_ip("00:1b:21:c1:fc:da".parse().unwrap(), BOOTSTRAP_INTERFACE_ID));
        // gravytrain ixgbe0
        bootstrap_discovery_addrs.insert(mac_to_bootstrap_ip("00:1b:21:c1:fd:24".parse().unwrap(), BOOTSTRAP_INTERFACE_ID));
        // frostypaws ixgbe3
        bootstrap_discovery_addrs.insert(mac_to_bootstrap_ip("80:61:5f:11:ab:31".parse().unwrap(), BOOTSTRAP_INTERFACE_ID));

        let rss_config = RssConfig {
            rack_subnet: "fd00:1122:3344:0100::".parse()?,

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
                user_name: UserId::try_from("admin").unwrap(),
                user_password_hash,
            },

            // no opte config if this is None!
            // rack_network_config: None,

            rack_network_config: Some(RackNetworkConfig {
                rack_subnet: "fd00:1122:3344:0100::".parse()?, // XXX dupe?

                // pool for switch ports
                infra_ip_first: "192.168.1.100".parse().unwrap(),
                infra_ip_last: "192.168.1.150".parse().unwrap(),

                ports: vec![
                    UplinkConfig {
                        // fancyfeast interface connected to 10G network
                        gateway_ip: "192.168.1.1".parse().unwrap(),

                        // the frostypaws uplink should have 192.168.1.1
                        switch: SwitchLocation::Switch0,

                        // the name of the qsfp interface to use for reaching the
                        // default gateway (generally qsfp0)
                        uplink_port: String::from("qsfp0"),

                        uplink_port_speed: PortSpeed::Speed10G,
                        uplink_port_fec: PortFec::None,

                        // address from the infra ip pool to assign to the qsfp port
                        uplink_cidr: "192.168.1.100/24".parse().unwrap(),

                        uplink_vid: None,
                    }.into(),

                    // XXX switch 1?
                ],

                bgp: vec![],
            }),
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

            request_body_max_bytes: 2147483648,

            ..ConfigDropshot::default()
        },

        log: ConfigLogging::File {
            level: ConfigLoggingLevel::Info,
            path: "/dev/stdout".into(),
            if_exists: ConfigLoggingIfExists::Append,
        },

        sled_mode: sled_mode.clone(),

        sidecar_revision: SidecarRevision::Physical(String::from("rev_a")),

        vlan: None,

        zpools: {
            // zpool list -o name | grep oxp_
            let cmd = Command::new("zpool")
                .arg("list")
                .arg("-o")
                .arg("name")
                .output()?;

            let text = String::from_utf8_lossy(&cmd.stdout);
            let mut pools = vec![];

            for line in text.split("\n") {
                let pool = line.trim();
                if pool.starts_with(ZPOOL_EXTERNAL_PREFIX) || pool.starts_with(ZPOOL_INTERNAL_PREFIX) {
                    eprintln!("discovered {}", pool);
                    pools.push(pool);
                }
            }

            if pools.is_empty() {
                None
            } else {
                Some(
                    pools
                        .into_iter()
                        .map(|x| ZpoolName::from_str(x).unwrap())
                        .collect(),
                )
            }
        },

        data_link: Some(PhysicalLink(String::from(match hostname.trim() {
            "dinnerbone" => "ixgbe0",
            "kibblesnbits" => "ixgbe0",
            "gravytrain" => "ixgbe0",
            "frostypaws" => "ixgbe3",
            _ => panic!("unknown hostname {}", hostname),
        }))),

        // data_links: [String; 2]
        data_links: if MULTI_SWITCH_MODE {
            match hostname.trim() {
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
            match hostname.trim() {
                "dinnerbone" => ["ixgbe0".to_string(), "net1".to_string()],
                "kibblesnbits" => ["ixgbe0".to_string(), "net1".to_string()],
                "gravytrain" => ["ixgbe0".to_string(), "net1".to_string()],
                "frostypaws" => ["ixgbe3".to_string(), "net1".to_string()],
                _ => panic!("unknown hostname {}", hostname),
            }
        },

        skip_timesync: Some(false),

        switch_zone_maghemite_links: match hostname.trim() {
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

        updates: ConfigUpdates::default(),

        // prod gimlets set 80% of their memory as a VMM reservoir, so match
        // that here. When propolis is compiled with the "omicron-build"
        // feature it will return 500 if there isn't enough reservoir present to
        // launch your instance.
        //
        // however, this seemed to panic some of my desktops. so go a little
        // lower.
        vmm_reservoir_percentage: Some(60),
        vmm_reservoir_size_mb: None,

        // If this is set, then the sled agent will try to configure swap for
        // us. I've already done this so it's not required.
        swap_device_size_gb: None,
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
                        multicast_addr: None,

                        // use first entry for switch 0, second for switch 1 MGS
                        // will grab based on bootstrap address, listen on same port
                        // for all
                        bind_addrs: Some([
                            "[::]:33300".parse().unwrap(),
                            "[::]:33301".parse().unwrap(),
                        ]),

                        serial_number: match hostname.trim() {
                            "frostypaws" => String::from("switch0"),
                            "kibblesnbits" => String::from("switch1"),

                            _ => panic!("unknown hostname {}", hostname.trim()),
                        },

                        // can ignore
                        manufacturing_root_cert_seed:
                            <[u8; 32]>::from_hex("01de01de01de01de01de01de01de01de01de01de01de01de01de01de01de01de").unwrap(),

                        // can ignore
                        device_id_cert_seed:
                            <[u8; 32]>::from_hex("01de000000000000000000000000000000000000000000000000000000000001").unwrap(),

                        components: vec![],
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

    // Simulated SP in the global zone for the sled
    let sp_sim_config = sp_sim::config::Config {
        simulated_sps: SimulatedSpsConfig {
            sidecar: vec![],

            gimlet: vec![
                sp_sim::config::GimletConfig { common: SpCommonConfig {
                    multicast_addr: None,

                    // use first entry for switch 0, second for switch 1 MGS
                    // will grab based on bootstrap address, listen on same port
                    // for all
                    bind_addrs: Some([
                        "[::]:33300".parse().unwrap(),
                        "[::]:33301".parse().unwrap(),
                    ]),

                    serial_number: hostname.trim().to_string(),

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
                    ],
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
                // sidecar 0
                omicron_gateway::SwitchPortDescription {
                    config: omicron_gateway::SwitchPortConfig::Simulated {
                        fake_interface: String::from("sidecar0"),
                        addr: SocketAddrV6::new(
                            "fdb0:8061:5f11:ab31::2".parse().unwrap(),
                            33300,
                            0,
                            0,
                        ),
                    },

                    ignition_target: 1,

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
                port.push(
                    omicron_gateway::SwitchPortDescription {
                        config: omicron_gateway::SwitchPortConfig::Simulated {
                            fake_interface: String::from("sidecar1"),
                            addr: SocketAddrV6::new(
                                "fdb0:1b:21c1:fcda::2".parse().unwrap(),
                                33301, // XXX match on hostname, switch ports here?
                                0,
                                0,
                            ),
                        },

                        ignition_target: 2,

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
                );
            }

            port.extend(vec![
                // dinnerbone
                omicron_gateway::SwitchPortDescription {
                    config: omicron_gateway::SwitchPortConfig::Simulated {
                        fake_interface: String::from("dinnerbone"),
                        // ixgbe0
                        addr: SocketAddrV6::new(
                            mac_to_bootstrap_ip("00:1b:21:c1:ff:e0".parse().unwrap(), BOOTSTRAP_INTERFACE_ID),
                            match hostname.trim() {
                                "frostypaws" => 33300,
                                "kibblesnbits" => 33301,
                                _ => panic!("unknown hostname {}", hostname),
                            },
                            0,
                            0,
                        ),
                    },

                    ignition_target: 3,

                    location: vec![
                        (
                            String::from("switch0"),
                            omicron_gateway::SpIdentifier {
                                typ: omicron_gateway::SpType::Sled,
                                slot: 0,
                            },
                        ),
                        (
                            String::from("switch1"),
                            omicron_gateway::SpIdentifier {
                                typ: omicron_gateway::SpType::Sled,
                                slot: 0,
                            },
                        ),
                    ].into_iter().collect(),
                },

                // kibblesnbits
                omicron_gateway::SwitchPortDescription {
                    config: omicron_gateway::SwitchPortConfig::Simulated {
                        fake_interface: String::from("kibblesnbits"),
                        // ixgbe0
                        addr: SocketAddrV6::new(
                            mac_to_bootstrap_ip("00:1b:21:c1:fc:da".parse().unwrap(), BOOTSTRAP_INTERFACE_ID),
                            match hostname.trim() {
                                "frostypaws" => 33300,
                                "kibblesnbits" => 33301,
                                _ => panic!("unknown hostname {}", hostname),
                            },
                            0,
                            0,
                        ),
                    },

                    ignition_target: 4,

                    location: vec![
                        (
                            String::from("switch0"),
                            omicron_gateway::SpIdentifier {
                                typ: omicron_gateway::SpType::Sled,
                                slot: 1,
                            },
                        ),
                        (
                            String::from("switch1"),
                            omicron_gateway::SpIdentifier {
                                typ: omicron_gateway::SpType::Sled,
                                slot: 1,
                            },
                        ),
                    ].into_iter().collect(),
                },

                // gravytrain
                omicron_gateway::SwitchPortDescription {
                    config: omicron_gateway::SwitchPortConfig::Simulated {
                        fake_interface: String::from("gravytrain"),
                        // ixgbe0
                        addr: SocketAddrV6::new(
                            mac_to_bootstrap_ip("00:1b:21:c1:fd:24".parse().unwrap(), BOOTSTRAP_INTERFACE_ID),
                            match hostname.trim() {
                                "frostypaws" => 33300,
                                "kibblesnbits" => 33301,
                                _ => panic!("unknown hostname {}", hostname),
                            },
                            0,
                            0,
                        ),
                    },

                    ignition_target: 5,

                    location: vec![
                        (
                            String::from("switch0"),
                            omicron_gateway::SpIdentifier {
                                typ: omicron_gateway::SpType::Sled,
                                slot: 2,
                            },
                        ),
                        (
                            String::from("switch1"),
                            omicron_gateway::SpIdentifier {
                                typ: omicron_gateway::SpType::Sled,
                                slot: 2,
                            },
                        ),
                    ].into_iter().collect(),
                },

                // frostypaws
                omicron_gateway::SwitchPortDescription {
                    config: omicron_gateway::SwitchPortConfig::Simulated {
                        fake_interface: String::from("frostypaws"),
                        // ixgbe3
                        addr: SocketAddrV6::new(
                            mac_to_bootstrap_ip("80:61:5f:11:ab:31".parse().unwrap(), BOOTSTRAP_INTERFACE_ID),
                            match hostname.trim() {
                                "frostypaws" => 33300,
                                "kibblesnbits" => 33301,
                                _ => panic!("unknown hostname {}", hostname),
                            },
                            0,
                            0,
                        ),
                    },

                    ignition_target: 6,

                    location: vec![
                        (
                            String::from("switch0"),
                            omicron_gateway::SpIdentifier {
                                typ: omicron_gateway::SpType::Sled,
                                slot: 3,
                            },
                        ),
                        (
                            String::from("switch1"),
                            omicron_gateway::SpIdentifier {
                                typ: omicron_gateway::SpType::Sled,
                                slot: 3,
                            },
                        ),
                    ].into_iter().collect(),
                },
            ]);

            port
        };

        let mgs_sim_config = omicron_gateway::Config {
            host_phase2_recovery_image_cache_max_images: 1,

            dropshot: omicron_gateway::PartialDropshotConfig {
                request_body_max_bytes: 536870912,
            },

            switch: omicron_gateway::SwitchConfig {
                udp_listen_port: 12225, // gateway_sp_comms::MGS_PORT

                // name of interface, not port
                local_ignition_controller_interface: match hostname.trim() {
                    "frostypaws" => String::from("sidecar0"),
                    "kibblesnbits" => String::from("sidecar1"),

                    _ => panic!("unknown hostname {}", hostname.trim()),
                },

                rpc_max_attempts: 5,
                rpc_per_attempt_timeout_millis: 2000,

                location: omicron_gateway::LocationConfig {
                    // name of location hashmap key
                    names: vec![
                        String::from("switch0"),
                        String::from("switch1"),
                    ],

                    // - the list of switch ports to contact to determine
                    //   location
                    // - each port is a subset of names vec above
                    determination: vec![
                        omicron_gateway::LocationDeterminationConfig {
                            interfaces: vec![
                                String::from("frostypaws"),
                                String::from("kibblesnbits"),
                            ],
                            sp_port_1: vec![String::from("switch0")],
                            sp_port_2: vec![String::from("switch1")],
                        }
                    ],
                },

                port,
            },

            log: ConfigLogging::File {
                level: ConfigLoggingLevel::Info,
                path: "/dev/stdout".into(),
                if_exists: ConfigLoggingIfExists::Append,
            },
        };

        std::fs::write(
            "smf/mgs-sim/config.toml",
            toml::to_string(&mgs_sim_config)?.as_bytes(),
        )?;
    }

    Ok(())
}