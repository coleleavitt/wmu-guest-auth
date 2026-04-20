use std::net::IpAddr;

use hickory_resolver::Resolver;
use hickory_resolver::proto::rr::RecordType;

use crate::error::WmuError;

#[derive(Debug)]
pub struct DnsRecord {
    pub name: String,
    pub record_type: String,
    pub value: String,
}

#[derive(Debug)]
pub struct DnsReport {
    pub records: Vec<DnsRecord>,
}

const DOMAINS: &[&str] = &[
    "wmich.edu",
    "legacy.wmich.edu",
    "virtual.wireless.wmich.edu",
    "wireless.wmich.edu",
    "auth.wmich.edu",
    "webauth.wmich.edu",
    "vpn.wmich.edu",
    "www.wmich.edu",
];

const RECORD_TYPES: &[(RecordType, &str)] = &[
    (RecordType::A, "A"),
    (RecordType::AAAA, "AAAA"),
    (RecordType::CNAME, "CNAME"),
    (RecordType::MX, "MX"),
    (RecordType::NS, "NS"),
    (RecordType::TXT, "TXT"),
    (RecordType::SOA, "SOA"),
    (RecordType::SRV, "SRV"),
    (RecordType::CAA, "CAA"),
];

const EXTRA_TXT: &[&str] = &["_dmarc.wmich.edu"];

pub async fn run_recon() -> Result<DnsReport, WmuError> {
    let resolver = Resolver::builder_tokio()
        .map_err(|e| WmuError::Dns(e.into()))?
        .build();
    let mut records = Vec::new();

    for &domain in DOMAINS {
        for &(rtype, label) in RECORD_TYPES {
            if let Ok(response) = resolver.lookup(domain, rtype).await {
                for record in response.iter() {
                    records.push(DnsRecord {
                        name: domain.to_string(),
                        record_type: label.to_string(),
                        value: record.to_string(),
                    });
                }
            }
        }
    }

    for &name in EXTRA_TXT {
        if let Ok(response) = resolver.txt_lookup(name).await {
            for txt in response.iter() {
                records.push(DnsRecord {
                    name: name.to_string(),
                    record_type: "TXT".to_string(),
                    value: txt.to_string(),
                });
            }
        }
    }

    for &domain in &[
        "legacy.wmich.edu",
        "virtual.wireless.wmich.edu",
        "auth.wmich.edu",
    ] {
        if let Ok(response) = resolver.lookup_ip(domain).await {
            for ip in response.iter() {
                if let IpAddr::V4(v4) = ip {
                    let arpa = format!(
                        "{}.{}.{}.{}.in-addr.arpa.",
                        v4.octets()[3],
                        v4.octets()[2],
                        v4.octets()[1],
                        v4.octets()[0]
                    );
                    if let Ok(ptr) = resolver.lookup(arpa.as_str(), RecordType::PTR).await {
                        for r in ptr.iter() {
                            records.push(DnsRecord {
                                name: domain.to_string(),
                                record_type: "PTR".to_string(),
                                value: r.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(DnsReport { records })
}
