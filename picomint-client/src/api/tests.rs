use std::collections::BTreeMap;
use std::str::FromStr as _;

use iroh_base::PublicKey;
use picomint_core::config::FederationId;
use picomint_core::invite_code::InviteCode;
use picomint_core::{NumPeersExt as _, PeerId};

fn test_node_id(byte: u8) -> PublicKey {
    PublicKey::from_bytes(&[byte; 32]).expect("valid public key")
}

#[test]
fn converts_invite_code() {
    let connect = InviteCode::new(test_node_id(0x11), PeerId::from(1), FederationId::dummy());

    let bech32 = connect.to_string();
    let connect_parsed = InviteCode::from_str(&bech32).expect("parses");
    assert_eq!(connect, connect_parsed);

    let json = serde_json::to_string(&connect).unwrap();
    let connect_as_string: String = serde_json::from_str(&json).unwrap();
    assert_eq!(connect_as_string, bech32);
    let connect_parsed_json: InviteCode = serde_json::from_str(&json).unwrap();
    assert_eq!(connect_parsed_json, connect_parsed);
}

#[test]
fn creates_essential_guardians_invite_code() {
    let mut peer_to_node_id_map = BTreeMap::new();
    peer_to_node_id_map.insert(PeerId::from(0), test_node_id(0x01));
    peer_to_node_id_map.insert(PeerId::from(1), test_node_id(0x02));
    peer_to_node_id_map.insert(PeerId::from(2), test_node_id(0x03));
    peer_to_node_id_map.insert(PeerId::from(3), test_node_id(0x04));
    let max_size = peer_to_node_id_map.to_num_peers().max_evil() + 1;

    let code =
        InviteCode::new_with_essential_num_guardians(&peer_to_node_id_map, FederationId::dummy());

    assert_eq!(FederationId::dummy(), code.federation_id());

    let expected_map: BTreeMap<PeerId, PublicKey> =
        peer_to_node_id_map.into_iter().take(max_size).collect();
    assert_eq!(expected_map, code.peers());
}
