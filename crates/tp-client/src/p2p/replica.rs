#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReplicaIdParts<'a> {
    family: &'a str,
    random: &'a str,
    index: usize,
}

fn parse_replica_id(client_id: &str) -> Option<ReplicaIdParts<'_>> {
    let (family, index) = client_id.rsplit_once('-')?;
    let (_, random) = family.rsplit_once('-')?;
    if family.is_empty()
        || random.is_empty()
        || random.len() != 8
        || !random.bytes().all(|b| b.is_ascii_alphanumeric())
        || !is_replica_index(index)
    {
        return None;
    }
    Some(ReplicaIdParts {
        family,
        random,
        index: index.parse().ok()?,
    })
}

pub(crate) fn replica_seed_for_tunnel<'a>(tunnel_id: &str, client_id: &'a str) -> Option<&'a str> {
    let parts = parse_replica_id(client_id)?;
    let expected_family = format!("{tunnel_id}-{}", parts.random);
    (parts.family == expected_family).then_some(parts.random)
}

pub(crate) fn replica_index(client_id: &str) -> Option<usize> {
    parse_replica_id(client_id).map(|parts| parts.index)
}

pub(crate) fn replica_id_for_index(client_id: &str, index: usize) -> Option<String> {
    let parts = parse_replica_id(client_id)?;
    Some(format!("{}-{index}", parts.family))
}

#[cfg(test)]
pub(crate) fn is_replica_suffix(suffix: &str) -> bool {
    is_replica_index(suffix)
}

fn is_replica_index(index: &str) -> bool {
    index == "0"
        || (!index.is_empty()
            && !index.starts_with('0')
            && index.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
pub(crate) fn replica_parent(client_id: &str) -> Option<String> {
    let parts = parse_replica_id(client_id)?;
    if parts.index == 0 {
        return None;
    }
    Some(format!("{}-0", parts.family))
}

pub(crate) fn same_or_child_replica(base_client_id: &str, client_id: &str) -> bool {
    if client_id == base_client_id {
        return true;
    }
    parse_replica_id(base_client_id)
        .zip(parse_replica_id(client_id))
        .map(|(base, child)| base.family == child.family)
        .unwrap_or(false)
}

pub(crate) fn replica_family_id(client_id: &str) -> String {
    parse_replica_id(client_id)
        .map(|parts| format!("{}-0", parts.family))
        .unwrap_or_else(|| client_id.to_string())
}

pub(crate) fn same_replica_family(a: &str, b: &str) -> bool {
    parse_replica_id(a)
        .zip(parse_replica_id(b))
        .map(|(a, b)| a.family == b.family)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_base_numeric_suffixes_do_not_share_family() {
        assert!(is_replica_suffix("1"));
        assert!(!same_or_child_replica("pc-1", "pc-1-1"));
        assert!(!same_replica_family("pc-1", "pc-1-1"));
        assert!(!same_replica_family("pc-1", "pc-2"));
        assert_eq!(replica_family_id("pc-1-1"), "pc-1-1");
    }

    #[test]
    fn new_format_replicas_share_family() {
        assert!(!is_replica_suffix("r1"));
        assert!(!same_or_child_replica(
            "pc-1-AbC12345-0",
            "pc-1-AbC12345-r1"
        ));
        assert!(same_or_child_replica("pc-1-AbC12345-0", "pc-1-AbC12345-1"));
        assert!(same_or_child_replica("pc-1-AbC12345-4", "pc-1-AbC12345-1"));
        assert!(same_replica_family("seed-7Neb0000-0", "seed-7Neb0000-1"));
        assert!(same_replica_family("pc-1-AbC12345-1", "pc-1-AbC12345-2"));
        assert!(!same_replica_family("pc-1-AbC12345-1", "pc-2-AbC12345-1"));
        assert!(!same_replica_family("pc-1-AbC12345-1", "pc-1-AbC123_-2"));
        assert!(!same_replica_family("pc-1-AbC12345-1", "pc-1-AbC12345-01"));
        assert_eq!(
            replica_parent("pc-1-AbC12345-1"),
            Some("pc-1-AbC12345-0".to_string())
        );
        assert_eq!(replica_family_id("pc-1-AbC12345-1"), "pc-1-AbC12345-0");
        assert_eq!(replica_family_id("pc-1-AbC12345-2"), "pc-1-AbC12345-0");
        assert_eq!(
            replica_id_for_index("pc-1-AbC12345-1", 3),
            Some("pc-1-AbC12345-3".to_string())
        );
        assert_eq!(replica_id_for_index("pc-1", 3), None);
    }

    #[test]
    fn replica_seed_is_scoped_to_the_exact_tunnel() {
        assert_eq!(
            replica_seed_for_tunnel("pc-1", "pc-1-AbC12345-7"),
            Some("AbC12345")
        );
        assert_eq!(replica_seed_for_tunnel("pc", "pc-1-AbC12345-7"), None);
        assert_eq!(replica_seed_for_tunnel("pc-1", "legacy-client"), None);
    }
}
