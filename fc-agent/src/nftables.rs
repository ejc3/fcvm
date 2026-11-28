//! The snapshot gate's removal, spoken to nf_tables directly over nfnetlink.
//!
//! The guest's `iptables` is the nf_tables backend, so the gate `close()`
//! installs with it lives in the `filter` table of the `ip` and `ip6`
//! families: two regular chains, and one jump rule in each of the `INPUT` and
//! `OUTPUT` base chains. Opening a half of the gate deletes, in both families,
//! that half's jump rules, then flushes and deletes its chain. This module
//! does that in ONE nf_tables transaction for both families: one batch, one
//! commit, no process spawn. `iptables-restore` can only commit one family per
//! process, so the same removal through it was two spawns and two commits per
//! half, and between the two commits one family was open and the other not.
//!
//! What it reads before the batch is the rule handles (one dump per base chain
//! per family) and the chain names (one dump per family). A rule handle names
//! one rule for the table's lifetime and is never reused, so a batch built
//! from those reads deletes exactly the rules it read or, if a rule has gone
//! since, fails as a whole: nf_tables aborts the transaction on the first
//! failing message and nothing is removed in either family.

use std::io;

use anyhow::{bail, Context, Result};

use crate::netlink::{
    append_netlink_header, decode_netlink_error, for_each_netlink_message, netlink_align,
    read_dump, NetlinkChannel, NetlinkHeader, OnInterrupt, NEXT_NETLINK_SEQUENCE, NLMSG_ERROR,
    NLM_F_ACK, NLM_F_CREATE, NLM_F_DUMP, NLM_F_EXCL, NLM_F_REQUEST,
};

/// `NETLINK_NETFILTER` from linux/netlink.h.
pub(crate) const NETLINK_NETFILTER: i32 = 12;

const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFNL_MSG_BATCH_BEGIN: u16 = 0x10;
const NFNL_MSG_BATCH_END: u16 = 0x11;
const NFNETLINK_V0: u8 = 0;

const NFT_MSG_NEWTABLE: u16 = 0;
const NFT_MSG_DELTABLE: u16 = 2;
const NFT_MSG_NEWCHAIN: u16 = 3;
const NFT_MSG_GETCHAIN: u16 = 4;
const NFT_MSG_DELCHAIN: u16 = 5;
const NFT_MSG_NEWRULE: u16 = 6;
const NFT_MSG_GETRULE: u16 = 7;
const NFT_MSG_DELRULE: u16 = 8;
const NFT_MSG_NEWSET: u16 = 9;
const NFT_MSG_NEWGEN: u16 = 15;
const NFT_MSG_GETGEN: u16 = 16;

const NFTA_GEN_ID: u16 = 1;
/// `NFNL_BATCH_GENID`: the ruleset generation a batch was built against.
const NFNL_BATCH_GENID: u16 = 1;

const NFTA_CHAIN_TABLE: u16 = 1;
const NFTA_CHAIN_NAME: u16 = 3;
const NFTA_RULE_TABLE: u16 = 1;
const NFTA_RULE_CHAIN: u16 = 2;
const NFTA_RULE_HANDLE: u16 = 3;
const NFTA_RULE_EXPRESSIONS: u16 = 4;
const NFTA_RULE_USERDATA: u16 = 7;
/// `NLA_F_NESTED`: the attribute holds attributes.
const NLA_F_NESTED: u16 = 0x8000;
const NFTA_LIST_ELEM: u16 = 1;
const NFTA_EXPR_NAME: u16 = 1;
const NFTA_EXPR_DATA: u16 = 2;
const NFTA_IMMEDIATE_DREG: u16 = 1;
const NFTA_IMMEDIATE_DATA: u16 = 2;
const NFTA_DATA_VERDICT: u16 = 2;
const NFTA_VERDICT_CODE: u16 = 1;
const NFTA_VERDICT_CHAIN: u16 = 2;

/// `NFT_REG_VERDICT`: an immediate that writes the verdict register.
const NFT_REG_VERDICT: u32 = 0;
/// `NFT_JUMP` from linux/netfilter/nf_tables.h.
const NFT_JUMP: i32 = -3;

/// The flags the kernel may set on an attribute type.
const NLA_TYPE_MASK: u16 = 0x3fff;

/// The table iptables-nft keeps its filter chains in, in both families.
const FILTER_TABLE: &str = "filter";

/// An nf_tables address family the gate lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Ipv4,
    Ipv6,
}

impl Family {
    pub(crate) const BOTH: [Family; 2] = [Family::Ipv4, Family::Ipv6];

    fn nfproto(self) -> u8 {
        match self {
            // NFPROTO_IPV4 and NFPROTO_IPV6.
            Family::Ipv4 => 2,
            Family::Ipv6 => 10,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Family::Ipv4 => "ip",
            Family::Ipv6 => "ip6",
        }
    }
}

/// One half of the gate as nf_tables holds it: the base chain the hook sits
/// in and the regular chain it jumps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GateHalf {
    pub(crate) hook: &'static str,
    pub(crate) chain: &'static str,
}

/// What one family's filter table holds of one gate half.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct HalfLayout {
    /// Handles of the rules in the base chain that are exactly a jump to the
    /// gate chain, in chain order. A jump carrying any match is not a hook.
    pub(crate) hooks: Vec<u64>,
    /// Whether the gate chain exists.
    pub(crate) chain: bool,
}

/// Bounded: more duplicate hooks than any interrupted close could leave is
/// tampering, and deleting without bound would hold the restore instead of
/// reporting it.
const MAX_DUPLICATE_HOOKS: usize = 64;

fn nft_type(message: u16) -> u16 {
    (NFNL_SUBSYS_NFTABLES << 8) | message
}

fn next_sequence() -> u32 {
    NEXT_NETLINK_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn append_attribute(bytes: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    let length = 4 + payload.len();
    bytes.extend_from_slice(&(length as u16).to_ne_bytes());
    bytes.extend_from_slice(&kind.to_ne_bytes());
    bytes.extend_from_slice(payload);
    bytes.resize(netlink_align(bytes.len()), 0);
}

fn append_string(bytes: &mut Vec<u8>, kind: u16, value: &str) {
    let mut payload = value.as_bytes().to_vec();
    payload.push(0);
    append_attribute(bytes, kind, &payload);
}

/// One nfnetlink message: header, `nfgenmsg`, attributes.
fn message(
    message_type: u16,
    flags: u16,
    sequence: u32,
    family: u8,
    res_id: u16,
    attributes: &[u8],
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(20 + attributes.len());
    append_netlink_header(
        &mut bytes,
        NetlinkHeader {
            length: (16 + 4 + attributes.len()) as u32,
            message_type,
            flags,
            sequence,
            port_id: 0,
        },
    );
    bytes.push(family);
    bytes.push(NFNETLINK_V0);
    bytes.extend_from_slice(&res_id.to_be_bytes());
    bytes.extend_from_slice(attributes);
    bytes
}

/// Walk the attributes in `bytes`, yielding (type without flags, payload).
fn attributes(bytes: &[u8]) -> Result<Vec<(u16, &[u8])>> {
    let mut found = Vec::new();
    let mut offset = 0usize;
    while offset + 4 <= bytes.len() {
        let length = u16::from_ne_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        let kind = u16::from_ne_bytes([bytes[offset + 2], bytes[offset + 3]]) & NLA_TYPE_MASK;
        if length < 4 || offset + length > bytes.len() {
            bail!("invalid nfnetlink attribute length {length} at offset {offset}");
        }
        found.push((kind, &bytes[offset + 4..offset + length]));
        offset += netlink_align(length);
    }
    Ok(found)
}

fn attribute<'a>(list: &[(u16, &'a [u8])], kind: u16) -> Option<&'a [u8]> {
    list.iter().find(|(k, _)| *k == kind).map(|(_, v)| *v)
}

fn string_attribute(payload: &[u8]) -> &[u8] {
    payload.split(|byte| *byte == 0).next().unwrap_or(payload)
}

fn be_u32(payload: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(payload.get(..4)?.try_into().ok()?))
}

/// Whether a rule's expressions are exactly a jump to `chain`, optionally
/// with the counter iptables-nft attaches to every rule.
fn rule_is_hook(expressions: &[u8], chain: &str) -> Result<bool> {
    let mut jump = false;
    for (kind, element) in attributes(expressions)? {
        if kind != NFTA_LIST_ELEM {
            return Ok(false);
        }
        let expression = attributes(element)?;
        let name = attribute(&expression, NFTA_EXPR_NAME).map(string_attribute);
        match name {
            Some(b"counter") => {}
            Some(b"immediate") if !jump => {
                let Some(data) = attribute(&expression, NFTA_EXPR_DATA) else {
                    return Ok(false);
                };
                let data = attributes(data)?;
                if attribute(&data, NFTA_IMMEDIATE_DREG).and_then(be_u32) != Some(NFT_REG_VERDICT) {
                    return Ok(false);
                }
                let Some(value) = attribute(&data, NFTA_IMMEDIATE_DATA) else {
                    return Ok(false);
                };
                let Some(verdict) = attribute(&attributes(value)?, NFTA_DATA_VERDICT) else {
                    return Ok(false);
                };
                let verdict = attributes(verdict)?;
                let code = attribute(&verdict, NFTA_VERDICT_CODE).and_then(be_u32);
                let target = attribute(&verdict, NFTA_VERDICT_CHAIN).map(string_attribute);
                if code != Some(NFT_JUMP as u32) || target != Some(chain.as_bytes()) {
                    return Ok(false);
                }
                jump = true;
            }
            _ => return Ok(false),
        }
    }
    Ok(jump)
}

fn dump<C: NetlinkChannel>(
    channel: &mut C,
    family: Family,
    request: u16,
    record: u16,
    attributes_bytes: &[u8],
    mut each: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let sequence = next_sequence();
    let request_type = nft_type(request);
    let datagram = message(
        request_type,
        NLM_F_REQUEST | NLM_F_DUMP,
        sequence,
        family.nfproto(),
        0,
        attributes_bytes,
    );
    channel.send(&datagram)?;
    read_dump(
        channel,
        sequence,
        request_type,
        nft_type(record),
        OnInterrupt::Refuse,
        |_, payload| {
            // Skip the nfgenmsg in front of the attributes.
            let body = payload.get(4..).context("short nf_tables record")?;
            each(body)
        },
        |_, _| Ok(()),
    )?;
    Ok(())
}

/// The namespace's current ruleset generation (`NFT_MSG_GETGEN`). Every
/// commit moves it. A batch that carries it (`removal_batch`) is refused with
/// `ERESTART` when any commit has landed since, so a removal built from reads
/// taken after this call cannot act on a ruleset that changed under it.
pub(crate) fn generation<C: NetlinkChannel>(channel: &mut C) -> Result<u32> {
    let sequence = next_sequence();
    let request_type = nft_type(NFT_MSG_GETGEN);
    channel.send(&message(request_type, NLM_F_REQUEST, sequence, 0, 0, &[]))?;
    loop {
        let datagram = channel
            .receive()
            .context("waiting for the nf_tables generation")?;
        let mut found: Option<Result<u32>> = None;
        for_each_netlink_message(datagram, |header, payload| {
            if header.sequence != sequence {
                return Ok(false);
            }
            if header.message_type == NLMSG_ERROR {
                let (error, _) = decode_netlink_error(payload)?;
                let errno = error.checked_neg().filter(|e| *e > 0).unwrap_or(libc::EIO);
                found = Some(Err(anyhow::anyhow!(
                    "reading the nf_tables generation: {}",
                    io::Error::from_raw_os_error(errno)
                )));
                return Ok(true);
            }
            if header.message_type == nft_type(NFT_MSG_NEWGEN) {
                let body = payload.get(4..).context("short nf_tables generation")?;
                let id = attribute(&attributes(body)?, NFTA_GEN_ID)
                    .and_then(be_u32)
                    .context("an nf_tables generation without an id")?;
                found = Some(Ok(id));
                return Ok(true);
            }
            Ok(false)
        })?;
        if let Some(found) = found {
            return found;
        }
    }
}

/// Read what `family`'s filter table holds of `half`: the hook rules' handles
/// and whether the chain exists. A family with no filter table holds nothing.
pub(crate) fn read_half<C: NetlinkChannel>(
    channel: &mut C,
    family: Family,
    half: GateHalf,
) -> Result<HalfLayout> {
    let mut layout = HalfLayout::default();
    let mut request = Vec::new();
    append_string(&mut request, NFTA_CHAIN_TABLE, FILTER_TABLE);
    dump(
        channel,
        family,
        NFT_MSG_GETCHAIN,
        NFT_MSG_NEWCHAIN,
        &request,
        |body| {
            let list = attributes(body)?;
            if attribute(&list, NFTA_CHAIN_TABLE).map(string_attribute)
                == Some(FILTER_TABLE.as_bytes())
                && attribute(&list, NFTA_CHAIN_NAME).map(string_attribute)
                    == Some(half.chain.as_bytes())
            {
                layout.chain = true;
            }
            Ok(())
        },
    )
    .with_context(|| format!("listing the {} filter chains", family.name()))?;
    let mut request = Vec::new();
    append_string(&mut request, NFTA_RULE_TABLE, FILTER_TABLE);
    append_string(&mut request, NFTA_RULE_CHAIN, half.hook);
    dump(
        channel,
        family,
        NFT_MSG_GETRULE,
        NFT_MSG_NEWRULE,
        &request,
        |body| {
            let list = attributes(body)?;
            if attribute(&list, NFTA_RULE_TABLE).map(string_attribute)
                != Some(FILTER_TABLE.as_bytes())
                || attribute(&list, NFTA_RULE_CHAIN).map(string_attribute)
                    != Some(half.hook.as_bytes())
            {
                return Ok(());
            }
            let handle = attribute(&list, NFTA_RULE_HANDLE)
                .and_then(|value| Some(u64::from_be_bytes(value.get(..8)?.try_into().ok()?)))
                .context("an nf_tables rule without a handle")?;
            if let Some(expressions) = attribute(&list, NFTA_RULE_EXPRESSIONS) {
                if rule_is_hook(expressions, half.chain)? {
                    layout.hooks.push(handle);
                }
            }
            Ok(())
        },
    )
    .with_context(|| format!("listing the {} filter {} rules", family.name(), half.hook))?;
    if layout.hooks.len() > MAX_DUPLICATE_HOOKS {
        bail!(
            "{} {} jumps to {} {} times (limit {MAX_DUPLICATE_HOOKS}); refusing cleanup",
            family.name(),
            half.hook,
            half.chain,
            layout.hooks.len()
        );
    }
    Ok(layout)
}

/// One planned removal: a family, a half, and what was read of it.
pub(crate) struct Removal {
    pub(crate) family: Family,
    pub(crate) half: GateHalf,
    pub(crate) layout: HalfLayout,
}

/// The batch that removes every `removals` entry, pinned to the ruleset
/// `generation` the removals were read under, and the sequence numbers of its
/// BEGIN and END messages. Per entry: the hook rules by handle, then the
/// chain's flush and deletion, the order iptables-restore uses. Returns `None`
/// when there is nothing to remove.
pub(crate) fn removal_batch(
    removals: &[Removal],
    udp_gate: bool,
    generation: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    let mut body = Vec::new();
    let mut messages = 0usize;
    for removal in removals {
        let family = removal.family.nfproto();
        for handle in &removal.layout.hooks {
            let mut attrs = Vec::new();
            append_string(&mut attrs, NFTA_RULE_TABLE, FILTER_TABLE);
            append_string(&mut attrs, NFTA_RULE_CHAIN, removal.half.hook);
            append_attribute(&mut attrs, NFTA_RULE_HANDLE, &handle.to_be_bytes());
            body.extend(message(
                nft_type(NFT_MSG_DELRULE),
                NLM_F_REQUEST,
                next_sequence(),
                family,
                0,
                &attrs,
            ));
            messages += 1;
        }
        if removal.layout.chain {
            let mut attrs = Vec::new();
            append_string(&mut attrs, NFTA_RULE_TABLE, FILTER_TABLE);
            append_string(&mut attrs, NFTA_RULE_CHAIN, removal.half.chain);
            body.extend(message(
                nft_type(NFT_MSG_DELRULE),
                NLM_F_REQUEST,
                next_sequence(),
                family,
                0,
                &attrs,
            ));
            let mut attrs = Vec::new();
            append_string(&mut attrs, NFTA_CHAIN_TABLE, FILTER_TABLE);
            append_string(&mut attrs, NFTA_CHAIN_NAME, removal.half.chain);
            body.extend(message(
                nft_type(NFT_MSG_DELCHAIN),
                NLM_F_REQUEST,
                next_sequence(),
                family,
                0,
                &attrs,
            ));
            messages += 2;
        }
    }
    if udp_gate {
        for family in Family::BOTH {
            body.extend(udp_gate_removal_messages(family));
            messages += 2;
        }
    }
    if messages == 0 {
        return None;
    }
    Some(pinned_batch(body, generation))
}

/// The table that holds the restored clone's UDP gate, in both families. It is
/// the gate's own: nothing else of the guest's firewall lives in it, and it is
/// deleted whole when the ingress half opens.
pub(crate) const UDP_GATE_TABLE: &str = "fcvm_snapshot_udp";
/// The set of peers the guest has sent UDP to since the gate was installed.
pub(crate) const UDP_GATE_SET: &str = "peers";
/// How long a peer stays in the set after the guest last sent to it. The
/// kernel counts it in the guest's run time.
const UDP_GATE_PEER_TIMEOUT_MS: u64 = 120_000;
/// The most peers the set holds. A reply from a peer that did not fit is
/// refused, as every datagram from outside is.
const UDP_GATE_PEER_LIMIT: u32 = 8192;

/// `NLM_F_APPEND`: a new rule goes after the last one.
const NLM_F_APPEND: u16 = 0x800;
const NFTA_TABLE_NAME: u16 = 1;
const NFTA_TABLE_FLAGS: u16 = 2;
const NFTA_CHAIN_HOOK: u16 = 4;
const NFTA_CHAIN_POLICY: u16 = 5;
const NFTA_CHAIN_TYPE: u16 = 7;
const NFTA_HOOK_HOOKNUM: u16 = 1;
const NFTA_HOOK_PRIORITY: u16 = 2;
const NFTA_SET_TABLE: u16 = 1;
const NFTA_SET_NAME: u16 = 2;
const NFTA_SET_FLAGS: u16 = 3;
const NFTA_SET_KEY_TYPE: u16 = 4;
const NFTA_SET_KEY_LEN: u16 = 5;
const NFTA_SET_DESC: u16 = 9;
const NFTA_SET_ID: u16 = 10;
const NFTA_SET_TIMEOUT: u16 = 11;
const NFTA_SET_DESC_SIZE: u16 = 1;
/// `NFT_SET_TIMEOUT | NFT_SET_EVAL`: elements expire, and rules add them.
const NFT_SET_TIMEOUT_AND_EVAL: u32 = 0x10 | 0x20;
const NF_INET_LOCAL_IN: u32 = 1;
const NF_INET_LOCAL_OUT: u32 = 3;
const NF_ACCEPT: u32 = 1;
const NF_DROP: u32 = 0;
/// `NFT_RETURN`: in a base chain, the packet continues to the next hook.
const NFT_RETURN: u32 = -5i32 as u32;
const NFTA_META_DREG: u16 = 1;
const NFTA_META_KEY: u16 = 2;
const NFT_META_L4PROTO: u32 = 16;
const NFT_META_IIFNAME: u32 = 6;
const NFTA_CMP_SREG: u16 = 1;
const NFTA_CMP_OP: u16 = 2;
const NFTA_CMP_DATA: u16 = 3;
const NFTA_DATA_VALUE: u16 = 1;
const NFTA_PAYLOAD_DREG: u16 = 1;
const NFTA_PAYLOAD_BASE: u16 = 2;
const NFTA_PAYLOAD_OFFSET: u16 = 3;
const NFTA_PAYLOAD_LEN: u16 = 4;
const NFT_PAYLOAD_NETWORK_HEADER: u32 = 1;
const NFT_PAYLOAD_TRANSPORT_HEADER: u32 = 2;
const NFTA_DYNSET_SET_NAME: u16 = 1;
const NFTA_DYNSET_SET_ID: u16 = 2;
const NFTA_DYNSET_OP: u16 = 3;
const NFTA_DYNSET_SREG_KEY: u16 = 4;
const NFT_DYNSET_OP_UPDATE: u32 = 1;
const NFTA_LOOKUP_SET: u16 = 1;
const NFTA_LOOKUP_SREG: u16 = 2;
const NFTA_LOOKUP_SET_ID: u16 = 4;
const IPPROTO_UDP: u8 = 17;

/// The first 16-byte register, and the 4-byte registers that follow it. The
/// key is the peer's address, then the peer's port, then the guest's own, each
/// port in a word of its own.
const NFT_REG_1: u32 = 1;
const NFT_REG32_01: u32 = 9;
const NFT_REG32_02: u32 = 10;
const NFT_REG32_04: u32 = 12;
const NFT_REG32_05: u32 = 13;

/// One rule of the restored clone's UDP gate, in the order a chain holds them.
///
/// The guest's own datagrams record where they went, and a datagram from a
/// peer the guest has not sent to is refused. A reply is a datagram from the
/// address and port the guest sent to, to the port it sent from. The set is
/// the gate's own, so no connection tracking is attached to the guest, and it
/// goes with the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UdpGateRule {
    /// Output: add `(destination address, destination port, source port)` of
    /// each UDP datagram to the set, and refresh the entry if it is there.
    Record,
    /// Input: a packet that came in on `lo` continues.
    PassLoopbackInterface,
    /// Input: a packet from a loopback address continues.
    PassLoopbackSource,
    /// Input: a UDP datagram from `(source address, source port)` to
    /// `destination port` that the set holds continues.
    PassKnownPeer,
    /// Input: every other UDP datagram is dropped, a fragment after the first
    /// of one excepted (it has no ports to judge, see the rule).
    DropUdp,
}

pub(crate) const UDP_GATE_OUTPUT: [UdpGateRule; 1] = [UdpGateRule::Record];
pub(crate) const UDP_GATE_INPUT: [UdpGateRule; 4] = [
    UdpGateRule::PassLoopbackInterface,
    UdpGateRule::PassLoopbackSource,
    UdpGateRule::PassKnownPeer,
    UdpGateRule::DropUdp,
];

/// A nested attribute built by `build`.
fn nested(bytes: &mut Vec<u8>, kind: u16, build: impl FnOnce(&mut Vec<u8>)) {
    let mut inner = Vec::new();
    build(&mut inner);
    append_attribute(bytes, kind | NLA_F_NESTED, &inner);
}

fn append_u32(bytes: &mut Vec<u8>, kind: u16, value: u32) {
    append_attribute(bytes, kind, &value.to_be_bytes());
}

/// One expression: its name and the attributes of its data.
fn expression(list: &mut Vec<u8>, name: &str, data: impl FnOnce(&mut Vec<u8>)) {
    nested(list, NFTA_LIST_ELEM, |element| {
        append_string(element, NFTA_EXPR_NAME, name);
        nested(element, NFTA_EXPR_DATA, data);
    });
}

fn meta_load(list: &mut Vec<u8>, key: u32, register: u32) {
    expression(list, "meta", |data| {
        append_u32(data, NFTA_META_KEY, key);
        append_u32(data, NFTA_META_DREG, register);
    });
}

fn compare_equal(list: &mut Vec<u8>, register: u32, value: &[u8]) {
    expression(list, "cmp", |data| {
        append_u32(data, NFTA_CMP_SREG, register);
        append_u32(data, NFTA_CMP_OP, 0);
        nested(data, NFTA_CMP_DATA, |inner| {
            append_attribute(inner, NFTA_DATA_VALUE, value)
        });
    });
}

fn payload_load(list: &mut Vec<u8>, base: u32, offset: u32, length: u32, register: u32) {
    expression(list, "payload", |data| {
        append_u32(data, NFTA_PAYLOAD_DREG, register);
        append_u32(data, NFTA_PAYLOAD_BASE, base);
        append_u32(data, NFTA_PAYLOAD_OFFSET, offset);
        append_u32(data, NFTA_PAYLOAD_LEN, length);
    });
}

fn verdict(list: &mut Vec<u8>, code: u32) {
    expression(list, "immediate", |data| {
        append_u32(data, NFTA_IMMEDIATE_DREG, NFT_REG_VERDICT);
        nested(data, NFTA_IMMEDIATE_DATA, |value| {
            nested(value, NFTA_DATA_VERDICT, |verdict| {
                append_u32(verdict, NFTA_VERDICT_CODE, code)
            });
        });
    });
}

/// Where a peer's key sits for one family: the address's width, and the
/// registers of the two ports.
struct UdpKeyLayout {
    address_length: u32,
    first_port: u32,
    second_port: u32,
    /// Offsets of the source and destination address in the network header.
    source: u32,
    destination: u32,
}

fn udp_key_layout(family: Family) -> UdpKeyLayout {
    match family {
        Family::Ipv4 => UdpKeyLayout {
            address_length: 4,
            first_port: NFT_REG32_01,
            second_port: NFT_REG32_02,
            source: 12,
            destination: 16,
        },
        Family::Ipv6 => UdpKeyLayout {
            address_length: 16,
            first_port: NFT_REG32_04,
            second_port: NFT_REG32_05,
            source: 8,
            destination: 24,
        },
    }
}

/// The key's type as `nft` writes it: the address and two ports, six bits each,
/// the address in the highest. Only userspace reads it.
fn udp_key_type(family: Family) -> u32 {
    const TYPE_INET_SERVICE: u32 = 13;
    let address = match family {
        Family::Ipv4 => 7,
        Family::Ipv6 => 8,
    };
    (address << 12) | (TYPE_INET_SERVICE << 6) | TYPE_INET_SERVICE
}

/// The key's length: the address, and two ports in a word each.
fn udp_key_length(family: Family) -> u32 {
    udp_key_layout(family).address_length + 8
}

/// The expressions of `rule`, as nf_tables takes them.
pub(crate) fn udp_rule_expressions(family: Family, rule: UdpGateRule) -> Vec<u8> {
    let mut list = Vec::new();
    let layout = udp_key_layout(family);
    match rule {
        UdpGateRule::Record => {
            meta_load(&mut list, NFT_META_L4PROTO, NFT_REG_1);
            compare_equal(&mut list, NFT_REG_1, &[IPPROTO_UDP]);
            payload_load(
                &mut list,
                NFT_PAYLOAD_NETWORK_HEADER,
                layout.destination,
                layout.address_length,
                NFT_REG_1,
            );
            // The remote port, then the local one.
            payload_load(
                &mut list,
                NFT_PAYLOAD_TRANSPORT_HEADER,
                2,
                2,
                layout.first_port,
            );
            payload_load(
                &mut list,
                NFT_PAYLOAD_TRANSPORT_HEADER,
                0,
                2,
                layout.second_port,
            );
            expression(&mut list, "dynset", |data| {
                append_string(data, NFTA_DYNSET_SET_NAME, UDP_GATE_SET);
                append_u32(data, NFTA_DYNSET_SET_ID, 1);
                append_u32(data, NFTA_DYNSET_OP, NFT_DYNSET_OP_UPDATE);
                append_u32(data, NFTA_DYNSET_SREG_KEY, NFT_REG_1);
            });
        }
        UdpGateRule::PassLoopbackInterface => {
            meta_load(&mut list, NFT_META_IIFNAME, NFT_REG_1);
            let mut name = [0u8; 16];
            name[..2].copy_from_slice(b"lo");
            compare_equal(&mut list, NFT_REG_1, &name);
            verdict(&mut list, NFT_RETURN);
        }
        UdpGateRule::PassLoopbackSource => {
            match family {
                Family::Ipv4 => {
                    payload_load(
                        &mut list,
                        NFT_PAYLOAD_NETWORK_HEADER,
                        layout.source,
                        1,
                        NFT_REG_1,
                    );
                    compare_equal(&mut list, NFT_REG_1, &[127]);
                }
                Family::Ipv6 => {
                    payload_load(
                        &mut list,
                        NFT_PAYLOAD_NETWORK_HEADER,
                        layout.source,
                        16,
                        NFT_REG_1,
                    );
                    let mut loopback = [0u8; 16];
                    loopback[15] = 1;
                    compare_equal(&mut list, NFT_REG_1, &loopback);
                }
            }
            verdict(&mut list, NFT_RETURN);
        }
        UdpGateRule::PassKnownPeer => {
            meta_load(&mut list, NFT_META_L4PROTO, NFT_REG_1);
            compare_equal(&mut list, NFT_REG_1, &[IPPROTO_UDP]);
            payload_load(
                &mut list,
                NFT_PAYLOAD_NETWORK_HEADER,
                layout.source,
                layout.address_length,
                NFT_REG_1,
            );
            payload_load(
                &mut list,
                NFT_PAYLOAD_TRANSPORT_HEADER,
                0,
                2,
                layout.first_port,
            );
            payload_load(
                &mut list,
                NFT_PAYLOAD_TRANSPORT_HEADER,
                2,
                2,
                layout.second_port,
            );
            expression(&mut list, "lookup", |data| {
                append_string(data, NFTA_LOOKUP_SET, UDP_GATE_SET);
                append_u32(data, NFTA_LOOKUP_SREG, NFT_REG_1);
                append_u32(data, NFTA_LOOKUP_SET_ID, 1);
            });
            verdict(&mut list, NFT_RETURN);
        }
        UdpGateRule::DropUdp => {
            meta_load(&mut list, NFT_META_L4PROTO, NFT_REG_1);
            compare_equal(&mut list, NFT_REG_1, &[IPPROTO_UDP]);
            // A load that fails ends the rule without a verdict. IPv6 reassembles
            // after this hook when nothing else in the guest does it earlier, and a
            // fragment after the first has no UDP header to read: the rule does not
            // drop it, as the lookup did not pass it. The datagram it belongs to is
            // whole only if its first fragment, which holds the ports, passed.
            payload_load(&mut list, NFT_PAYLOAD_TRANSPORT_HEADER, 0, 2, NFT_REG_1);
            verdict(&mut list, NF_DROP);
        }
    }
    list
}

/// The names of the expressions in `expressions`, in order.
#[cfg(test)]
pub(crate) fn expression_names(expressions: &[u8]) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for (kind, element) in attributes(expressions)? {
        if kind != NFTA_LIST_ELEM {
            bail!("an nf_tables rule expression list holds attribute {kind}");
        }
        let expression = attributes(element)?;
        let name = attribute(&expression, NFTA_EXPR_NAME)
            .map(string_attribute)
            .context("an nf_tables expression without a name")?;
        names.push(String::from_utf8_lossy(name).into_owned());
    }
    Ok(names)
}

fn table_message(message_type: u16, flags: u16, family: Family) -> Vec<u8> {
    let mut attrs = Vec::new();
    append_string(&mut attrs, NFTA_TABLE_NAME, UDP_GATE_TABLE);
    if message_type == NFT_MSG_NEWTABLE {
        append_u32(&mut attrs, NFTA_TABLE_FLAGS, 0);
    }
    message(
        nft_type(message_type),
        flags,
        next_sequence(),
        family.nfproto(),
        0,
        &attrs,
    )
}

/// The messages that make the UDP gate's table in one family: the table, the
/// set, and an output and an input chain with their rules.
fn udp_gate_install_messages(family: Family) -> Vec<u8> {
    let mut body = table_message(NFT_MSG_NEWTABLE, NLM_F_REQUEST | NLM_F_EXCL, family);
    let mut attrs = Vec::new();
    append_string(&mut attrs, NFTA_SET_TABLE, UDP_GATE_TABLE);
    append_string(&mut attrs, NFTA_SET_NAME, UDP_GATE_SET);
    append_u32(&mut attrs, NFTA_SET_FLAGS, NFT_SET_TIMEOUT_AND_EVAL);
    append_u32(&mut attrs, NFTA_SET_KEY_TYPE, udp_key_type(family));
    append_u32(&mut attrs, NFTA_SET_KEY_LEN, udp_key_length(family));
    append_u32(&mut attrs, NFTA_SET_ID, 1);
    nested(&mut attrs, NFTA_SET_DESC, |desc| {
        append_u32(desc, NFTA_SET_DESC_SIZE, UDP_GATE_PEER_LIMIT)
    });
    append_attribute(
        &mut attrs,
        NFTA_SET_TIMEOUT,
        &UDP_GATE_PEER_TIMEOUT_MS.to_be_bytes(),
    );
    body.extend(message(
        nft_type(NFT_MSG_NEWSET),
        NLM_F_REQUEST | NLM_F_CREATE,
        next_sequence(),
        family.nfproto(),
        0,
        &attrs,
    ));
    for (chain, hook, rules) in [
        ("out", NF_INET_LOCAL_OUT, &UDP_GATE_OUTPUT[..]),
        ("in", NF_INET_LOCAL_IN, &UDP_GATE_INPUT[..]),
    ] {
        let mut attrs = Vec::new();
        append_string(&mut attrs, NFTA_CHAIN_TABLE, UDP_GATE_TABLE);
        append_string(&mut attrs, NFTA_CHAIN_NAME, chain);
        nested(&mut attrs, NFTA_CHAIN_HOOK, |inner| {
            append_u32(inner, NFTA_HOOK_HOOKNUM, hook);
            append_u32(inner, NFTA_HOOK_PRIORITY, 0);
        });
        append_u32(&mut attrs, NFTA_CHAIN_POLICY, NF_ACCEPT);
        append_string(&mut attrs, NFTA_CHAIN_TYPE, "filter");
        body.extend(message(
            nft_type(NFT_MSG_NEWCHAIN),
            NLM_F_REQUEST | NLM_F_CREATE,
            next_sequence(),
            family.nfproto(),
            0,
            &attrs,
        ));
        for rule in rules {
            let mut attrs = Vec::new();
            append_string(&mut attrs, NFTA_RULE_TABLE, UDP_GATE_TABLE);
            append_string(&mut attrs, NFTA_RULE_CHAIN, chain);
            append_attribute(
                &mut attrs,
                NFTA_RULE_EXPRESSIONS | NLA_F_NESTED,
                &udp_rule_expressions(family, *rule),
            );
            body.extend(message(
                nft_type(NFT_MSG_NEWRULE),
                NLM_F_REQUEST | NLM_F_CREATE | NLM_F_APPEND,
                next_sequence(),
                family.nfproto(),
                0,
                &attrs,
            ));
        }
    }
    body
}

/// The batch that installs the UDP gate in both families, and the sequence
/// numbers of its BEGIN and END. It reads no handle, so it is pinned to no
/// generation. A table of that name that already exists refuses it whole.
pub(crate) fn udp_gate_install_batch() -> (Vec<u8>, u32, u32) {
    let mut body = Vec::new();
    for family in Family::BOTH {
        body.extend(udp_gate_install_messages(family));
    }
    pinned_batch(body, 0)
}

/// The two messages that delete the UDP gate's table in one family whether or
/// not it is there: adding a table that exists changes nothing, and the
/// deletion takes its chains, rules and set with it.
fn udp_gate_removal_messages(family: Family) -> Vec<u8> {
    let mut messages = table_message(NFT_MSG_NEWTABLE, NLM_F_REQUEST, family);
    messages.extend(table_message(NFT_MSG_DELTABLE, NLM_F_REQUEST, family));
    messages
}

/// `body` between a BEGIN pinned to `generation` and an END that asks for the
/// acknowledgement, and the sequence numbers of the two.
fn pinned_batch(body: Vec<u8>, generation: u32) -> (Vec<u8>, u32, u32) {
    let begin = next_sequence();
    let mut pinned = Vec::new();
    append_attribute(&mut pinned, NFNL_BATCH_GENID, &generation.to_be_bytes());
    let mut batch = message(
        NFNL_MSG_BATCH_BEGIN,
        NLM_F_REQUEST,
        begin,
        0,
        NFNL_SUBSYS_NFTABLES,
        &pinned,
    );
    batch.extend(body);
    let end = next_sequence();
    batch.extend(message(
        NFNL_MSG_BATCH_END,
        NLM_F_REQUEST | NLM_F_ACK,
        end,
        0,
        NFNL_SUBSYS_NFTABLES,
        &[],
    ));
    (batch, begin, end)
}

/// A rule as nf_tables dumps it: its expression list and its user data, both
/// as the kernel encoded them. iptables-nft writes both, and replaying them in
/// `NFT_MSG_NEWRULE` writes the same rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuleBody {
    pub(crate) expressions: Vec<u8>,
    pub(crate) userdata: Option<Vec<u8>>,
}

/// Each expression of a rule as (name, data).
type Expressions = Vec<(Vec<u8>, Vec<u8>)>;

impl RuleBody {
    /// The rule without what traffic changes in it: each expression's name and
    /// data, with the data of iptables-nft's `counter` left out.
    fn canonical(&self) -> Result<(Expressions, Option<&[u8]>)> {
        let mut expressions = Vec::new();
        for (kind, element) in attributes(&self.expressions)? {
            if kind != NFTA_LIST_ELEM {
                bail!("an nf_tables rule expression list holds attribute {kind}");
            }
            let expression = attributes(element)?;
            let name = attribute(&expression, NFTA_EXPR_NAME)
                .map(string_attribute)
                .context("an nf_tables expression without a name")?
                .to_vec();
            let data = if name == b"counter" {
                Vec::new()
            } else {
                attribute(&expression, NFTA_EXPR_DATA)
                    .unwrap_or_default()
                    .to_vec()
            };
            expressions.push((name, data));
        }
        Ok((expressions, self.userdata.as_deref()))
    }

    /// Whether this rule is exactly a jump to `chain` (see `rule_is_hook`).
    pub(crate) fn is_hook(&self, chain: &str) -> Result<bool> {
        rule_is_hook(&self.expressions, chain)
    }
}

/// One rule of a chain, with the handle that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChainRule {
    pub(crate) handle: u64,
    pub(crate) body: RuleBody,
}

/// Whether `observed` holds exactly the rules of `expected`, in order, by
/// `RuleBody::canonical`.
pub(crate) fn same_rules(observed: &[ChainRule], expected: &[RuleBody]) -> Result<bool> {
    if observed.len() != expected.len() {
        return Ok(false);
    }
    for (rule, wanted) in observed.iter().zip(expected) {
        if rule.body.canonical()? != wanted.canonical()? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The rules of `chain` in `family`'s filter table, in chain order. A chain or
/// table that does not exist holds none.
pub(crate) fn read_rules<C: NetlinkChannel>(
    channel: &mut C,
    family: Family,
    chain: &str,
) -> Result<Vec<ChainRule>> {
    let mut rules = Vec::new();
    let mut request = Vec::new();
    append_string(&mut request, NFTA_RULE_TABLE, FILTER_TABLE);
    append_string(&mut request, NFTA_RULE_CHAIN, chain);
    dump(
        channel,
        family,
        NFT_MSG_GETRULE,
        NFT_MSG_NEWRULE,
        &request,
        |body| {
            let list = attributes(body)?;
            if attribute(&list, NFTA_RULE_TABLE).map(string_attribute)
                != Some(FILTER_TABLE.as_bytes())
                || attribute(&list, NFTA_RULE_CHAIN).map(string_attribute) != Some(chain.as_bytes())
            {
                return Ok(());
            }
            let handle = attribute(&list, NFTA_RULE_HANDLE)
                .and_then(|value| Some(u64::from_be_bytes(value.get(..8)?.try_into().ok()?)))
                .context("an nf_tables rule without a handle")?;
            rules.push(ChainRule {
                handle,
                body: RuleBody {
                    expressions: attribute(&list, NFTA_RULE_EXPRESSIONS)
                        .unwrap_or_default()
                        .to_vec(),
                    userdata: attribute(&list, NFTA_RULE_USERDATA).map(<[u8]>::to_vec),
                },
            });
            Ok(())
        },
    )
    .with_context(|| format!("listing the {} filter {chain} rules", family.name()))?;
    Ok(rules)
}

/// What became of a batch that nf_tables did not refuse outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Commit {
    Committed,
    /// The ruleset generation moved since the batch's reads; nothing changed.
    GenerationMoved,
}

/// Send a batch and wait for its verdict. nf_tables answers a committed batch
/// with an acknowledgement of its END message (the only message that asks for
/// one), and a failed one with an error naming the message that failed or,
/// for a failed commit or a moved generation, the BEGIN message; a failed
/// batch changes nothing.
pub(crate) fn commit<C: NetlinkChannel>(channel: &mut C, batch: &[u8], end: u32) -> Result<Commit> {
    channel.send(batch).context("sending the nf_tables batch")?;
    loop {
        let datagram = channel
            .receive()
            .context("waiting for the nf_tables batch verdict")?;
        let mut verdict: Option<Result<Commit>> = None;
        for_each_netlink_message(datagram, |header, payload| {
            if header.message_type != NLMSG_ERROR {
                return Ok(false);
            }
            let (error, request) = decode_netlink_error(payload)?;
            if error != 0 {
                let errno = error
                    .checked_neg()
                    .filter(|errno| *errno > 0)
                    .unwrap_or(libc::EIO);
                if errno == libc::ERESTART && request.message_type == NFNL_MSG_BATCH_BEGIN {
                    verdict = Some(Ok(Commit::GenerationMoved));
                    return Ok(true);
                }
                verdict = Some(Err(anyhow::anyhow!(
                    "nf_tables refused the batch at message type {:#x} sequence {}: {}",
                    request.message_type,
                    request.sequence,
                    io::Error::from_raw_os_error(errno)
                )));
                return Ok(true);
            }
            if request.sequence == end && request.message_type == NFNL_MSG_BATCH_END {
                verdict = Some(Ok(Commit::Committed));
                return Ok(true);
            }
            Ok(false)
        })?;
        if let Some(verdict) = verdict {
            return verdict;
        }
    }
}

#[cfg(test)]
pub(crate) mod model {
    //! A model of nf_tables' filter tables in both families, speaking the
    //! same nfnetlink messages as the kernel: chain and rule dumps, and
    //! batches that apply whole or not at all.

    use super::*;
    use crate::netlink::NLMSG_DONE;
    use std::collections::VecDeque;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Rule {
        /// `-j <chain>` with iptables-nft's counter.
        Jump(&'static str),
        /// Anything else, e.g. a jump that carries a match.
        Other,
        /// A rule exactly as written, e.g. one iptables-nft wrote.
        Body(RuleBody),
    }

    #[derive(Debug, Default, Clone, PartialEq, Eq)]
    pub(crate) struct Table {
        pub(crate) chains: Vec<&'static str>,
        /// (chain, handle, rule)
        pub(crate) rules: Vec<(&'static str, u64, Rule)>,
    }

    /// The UDP gate's table in one family, as the model kernel holds it.
    #[derive(Debug, Default, Clone, PartialEq, Eq)]
    pub(crate) struct UdpTable {
        pub(crate) set: bool,
        /// (chain name, hook number)
        pub(crate) chains: Vec<(String, u32)>,
        /// (chain name, the rule's expressions as sent)
        pub(crate) rules: Vec<(String, Vec<u8>)>,
    }

    /// A change to the model kernel, run while it holds itself.
    pub(crate) type KernelChange = Box<dyn FnOnce(&mut Kernel) + Send>;

    #[derive(Default)]
    pub(crate) struct Kernel {
        pub(crate) ipv4: Table,
        pub(crate) ipv6: Table,
        /// The UDP gate's table in `ip` and `ip6`, when one exists.
        pub(crate) udp_ipv4: Option<UdpTable>,
        pub(crate) udp_ipv6: Option<UdpTable>,
        /// Every committed batch, as the list of (family, operation) it applied.
        pub(crate) commits: Vec<Vec<String>>,
        pub(crate) refused: usize,
        /// While set, closing a channel waits for a message on it (up to two
        /// seconds), as a real close waits out an RCU grace period.
        pub(crate) close_blocker:
            Option<std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>>,
        /// The ruleset generation, moved by every commit.
        pub(crate) genid: u32,
        /// Runs once when the next batch arrives, before it is applied: a
        /// concurrent ruleset change between a removal's read and its commit.
        pub(crate) before_batch: Option<KernelChange>,
        /// Channels opened and closed so far.
        pub(crate) opens: usize,
        pub(crate) closes: usize,
        next_handle: u64,
        pending: VecDeque<Vec<u8>>,
    }

    impl Kernel {
        pub(crate) fn table(&mut self, family: u8) -> &mut Table {
            if family == 2 {
                &mut self.ipv4
            } else {
                &mut self.ipv6
            }
        }

        /// The armed gate in one family, as iptables-nft lays it out.
        pub(crate) fn arm(&mut self, family: Family) {
            // nf_tables never reports generation 0, and a batch pinned to 0
            // skips the check.
            self.genid = self.genid.max(1);
            let handles = [
                self.next_handle + 1,
                self.next_handle + 2,
                self.next_handle + 3,
            ];
            self.next_handle += 3;
            let table = self.table(family.nfproto());
            table.chains = vec![
                "INPUT",
                "FORWARD",
                "OUTPUT",
                "FCVM_SNAPSHOT_IN",
                "FCVM_SNAPSHOT_OUT",
            ];
            table.rules = vec![
                ("INPUT", handles[0], Rule::Jump("FCVM_SNAPSHOT_IN")),
                ("OUTPUT", handles[1], Rule::Jump("FCVM_SNAPSHOT_OUT")),
                ("FCVM_SNAPSHOT_IN", handles[2], Rule::Other),
            ];
        }

        /// The armed gate in one family with each gate chain holding `input`
        /// and `output` as written.
        pub(crate) fn arm_with(&mut self, family: Family, input: &[RuleBody], output: &[RuleBody]) {
            self.arm(family);
            self.table(family.nfproto())
                .rules
                .retain(|(c, _, _)| *c == "INPUT" || *c == "OUTPUT");
            for (chain, rules) in [("FCVM_SNAPSHOT_IN", input), ("FCVM_SNAPSHOT_OUT", output)] {
                for body in rules {
                    self.add_rule(family, chain, Rule::Body(body.clone()));
                }
            }
        }

        /// Replace the rule with `handle` in place, keeping its handle, as
        /// `NFT_MSG_NEWRULE` with `NLM_F_REPLACE` does, and move the generation.
        pub(crate) fn replace_rule(&mut self, family: Family, handle: u64, rule: Rule) {
            let table = self.table(family.nfproto());
            let slot = table
                .rules
                .iter_mut()
                .find(|(_, h, _)| *h == handle)
                .expect("the rule to replace exists");
            slot.2 = rule;
            self.genid += 1;
        }

        pub(crate) fn add_rule(&mut self, family: Family, chain: &'static str, rule: Rule) -> u64 {
            self.next_handle += 1;
            let handle = self.next_handle;
            self.table(family.nfproto())
                .rules
                .push((chain, handle, rule));
            handle
        }
    }

    /// A rule body with one expression whose data is `tag`, and iptables-nft's
    /// counter at `packets`: two bodies with one tag differ only in what
    /// traffic changes.
    pub(crate) fn body(tag: u8, packets: u64) -> RuleBody {
        let mut list = Vec::new();
        let mut counter_data = Vec::new();
        append_attribute(&mut counter_data, 1, &(packets * 64).to_be_bytes());
        append_attribute(&mut counter_data, 2, &packets.to_be_bytes());
        let mut counter = Vec::new();
        append_string(&mut counter, NFTA_EXPR_NAME, "counter");
        append_attribute(&mut counter, NFTA_EXPR_DATA | NLA_F_NESTED, &counter_data);
        append_attribute(&mut list, NFTA_LIST_ELEM | NLA_F_NESTED, &counter);
        let mut expression = Vec::new();
        append_string(&mut expression, NFTA_EXPR_NAME, "match");
        append_attribute(
            &mut expression,
            NFTA_EXPR_DATA | NLA_F_NESTED,
            &[tag, 0, 0, 0],
        );
        append_attribute(&mut list, NFTA_LIST_ELEM | NLA_F_NESTED, &expression);
        RuleBody {
            expressions: list,
            userdata: None,
        }
    }

    fn rule_record(family: u8, chain: &str, handle: u64, rule: &Rule, sequence: u32) -> Vec<u8> {
        let mut attrs = Vec::new();
        append_string(&mut attrs, NFTA_RULE_TABLE, FILTER_TABLE);
        append_string(&mut attrs, NFTA_RULE_CHAIN, chain);
        append_attribute(&mut attrs, NFTA_RULE_HANDLE, &handle.to_be_bytes());
        if let Rule::Body(body) = rule {
            append_attribute(&mut attrs, NFTA_RULE_EXPRESSIONS, &body.expressions);
            if let Some(userdata) = &body.userdata {
                append_attribute(&mut attrs, NFTA_RULE_USERDATA, userdata);
            }
            return message(nft_type(NFT_MSG_NEWRULE), 0x2, sequence, family, 0, &attrs);
        }
        let mut list = Vec::new();
        let mut counter = Vec::new();
        append_string(&mut counter, NFTA_EXPR_NAME, "counter");
        append_attribute(&mut list, NFTA_LIST_ELEM | 0x8000, &counter);
        if let Rule::Jump(target) = rule {
            let mut verdict = Vec::new();
            append_attribute(
                &mut verdict,
                NFTA_VERDICT_CODE,
                &(NFT_JUMP as u32).to_be_bytes(),
            );
            append_string(&mut verdict, NFTA_VERDICT_CHAIN, target);
            let mut value = Vec::new();
            append_attribute(&mut value, NFTA_DATA_VERDICT | 0x8000, &verdict);
            let mut data = Vec::new();
            append_attribute(
                &mut data,
                NFTA_IMMEDIATE_DREG,
                &NFT_REG_VERDICT.to_be_bytes(),
            );
            append_attribute(&mut data, NFTA_IMMEDIATE_DATA | 0x8000, &value);
            let mut expression = Vec::new();
            append_string(&mut expression, NFTA_EXPR_NAME, "immediate");
            append_attribute(&mut expression, NFTA_EXPR_DATA | 0x8000, &data);
            append_attribute(&mut list, NFTA_LIST_ELEM | 0x8000, &expression);
        } else {
            let mut expression = Vec::new();
            append_string(&mut expression, NFTA_EXPR_NAME, "match");
            append_attribute(&mut list, NFTA_LIST_ELEM | 0x8000, &expression);
        }
        append_attribute(&mut attrs, NFTA_RULE_EXPRESSIONS | 0x8000, &list);
        message(nft_type(NFT_MSG_NEWRULE), 0x2, sequence, family, 0, &attrs)
    }

    fn done(sequence: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        append_netlink_header(
            &mut bytes,
            NetlinkHeader {
                length: 20,
                message_type: NLMSG_DONE,
                flags: 0x2,
                sequence,
                port_id: 0,
            },
        );
        bytes.extend_from_slice(&0i32.to_ne_bytes());
        bytes
    }

    fn error(request: NetlinkHeader, errno: i32) -> Vec<u8> {
        let mut bytes = Vec::new();
        append_netlink_header(
            &mut bytes,
            NetlinkHeader {
                length: 36,
                message_type: NLMSG_ERROR,
                flags: 0,
                sequence: request.sequence,
                port_id: 0,
            },
        );
        bytes.extend_from_slice(&(-errno).to_ne_bytes());
        append_netlink_header(&mut bytes, request);
        bytes
    }

    /// One message about the UDP gate's table: the table, its set, a chain, a
    /// rule, or the table's deletion, as nf_tables answers it.
    fn apply_udp_message(
        table: &mut Option<UdpTable>,
        header: &NetlinkHeader,
        list: &[(u16, &[u8])],
        label: &str,
    ) -> std::result::Result<String, i32> {
        let name = |kind| {
            attribute(list, kind)
                .map(|value| String::from_utf8_lossy(string_attribute(value)).into_owned())
        };
        match header.message_type & 0xff {
            NFT_MSG_NEWTABLE => {
                if table.is_some() {
                    return if header.flags & NLM_F_EXCL != 0 {
                        Err(libc::EEXIST)
                    } else {
                        Ok(format!("{label} keep table {UDP_GATE_TABLE}"))
                    };
                }
                *table = Some(UdpTable::default());
                Ok(format!("{label} add table {UDP_GATE_TABLE}"))
            }
            NFT_MSG_DELTABLE => match table.take() {
                Some(_) => Ok(format!("{label} delete table {UDP_GATE_TABLE}")),
                None => Err(libc::ENOENT),
            },
            NFT_MSG_NEWSET => {
                let table = table.as_mut().ok_or(libc::ENOENT)?;
                if name(NFTA_SET_NAME).as_deref() != Some(UDP_GATE_SET) {
                    return Err(libc::EINVAL);
                }
                table.set = true;
                Ok(format!("{label} add set {UDP_GATE_SET}"))
            }
            NFT_MSG_NEWCHAIN => {
                let table = table.as_mut().ok_or(libc::ENOENT)?;
                let chain = name(NFTA_CHAIN_NAME).ok_or(libc::EINVAL)?;
                let hook = attribute(list, NFTA_CHAIN_HOOK)
                    .and_then(|hook| {
                        attribute(&attributes(hook).ok()?, NFTA_HOOK_HOOKNUM).and_then(be_u32)
                    })
                    .ok_or(libc::EINVAL)?;
                table.chains.push((chain.clone(), hook));
                Ok(format!("{label} add chain {chain}"))
            }
            NFT_MSG_NEWRULE => {
                let table = table.as_mut().ok_or(libc::ENOENT)?;
                let chain = name(NFTA_RULE_CHAIN).ok_or(libc::EINVAL)?;
                if !table.chains.iter().any(|(c, _)| *c == chain) {
                    return Err(libc::ENOENT);
                }
                let expressions = attribute(list, NFTA_RULE_EXPRESSIONS)
                    .unwrap_or_default()
                    .to_vec();
                table.rules.push((chain.clone(), expressions));
                Ok(format!("{label} add rule {chain}"))
            }
            _ => Err(libc::EINVAL),
        }
    }

    pub(crate) struct Channel {
        pub(crate) kernel: std::sync::Arc<std::sync::Mutex<Kernel>>,
        buffer: Vec<u8>,
    }

    impl Channel {
        pub(crate) fn new(kernel: std::sync::Arc<std::sync::Mutex<Kernel>>) -> Self {
            Self {
                kernel,
                buffer: Vec::new(),
            }
        }
    }

    impl Drop for Channel {
        fn drop(&mut self) {
            let blocker = self.kernel.lock().unwrap().close_blocker.clone();
            if let Some(blocker) = blocker {
                let _ = blocker
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(2));
            }
            self.kernel.lock().unwrap().closes += 1;
        }
    }

    impl NetlinkChannel for Channel {
        fn send(&mut self, datagram: &[u8]) -> Result<()> {
            let mut kernel = self.kernel.lock().unwrap();
            let mut messages = Vec::new();
            for_each_netlink_message(datagram, |header, payload| {
                messages.push((header, payload.to_vec()));
                Ok(false)
            })?;
            let (first, _) = messages[0];
            if first.message_type == NFNL_MSG_BATCH_BEGIN {
                if let Some(change) = kernel.before_batch.take() {
                    change(&mut kernel);
                }
                let pinned = attributes(&messages[0].1[4..])?
                    .iter()
                    .find(|(kind, _)| *kind == NFNL_BATCH_GENID)
                    .and_then(|(_, value)| be_u32(value));
                if pinned.is_some_and(|genid| genid != 0 && genid != kernel.genid) {
                    kernel.refused += 1;
                    kernel.pending.push_back(error(first, libc::ERESTART));
                    return Ok(());
                }
                let mut working = (
                    kernel.ipv4.clone(),
                    kernel.ipv6.clone(),
                    kernel.udp_ipv4.clone(),
                    kernel.udp_ipv6.clone(),
                );
                let mut applied = Vec::new();
                let mut failure = None;
                for (header, payload) in &messages[1..] {
                    if header.message_type == NFNL_MSG_BATCH_END {
                        break;
                    }
                    let family = payload[0];
                    let list = attributes(&payload[4..])?;
                    let label = if family == 2 { "ip" } else { "ip6" };
                    if attribute(&list, NFTA_TABLE_NAME).map(string_attribute)
                        == Some(UDP_GATE_TABLE.as_bytes())
                    {
                        let udp = if family == 2 {
                            &mut working.2
                        } else {
                            &mut working.3
                        };
                        match apply_udp_message(udp, header, &list, label) {
                            Ok(op) => applied.push(op),
                            Err(errno) => {
                                failure = Some(error(*header, errno));
                                break;
                            }
                        }
                        continue;
                    }
                    let table = if family == 2 {
                        &mut working.0
                    } else {
                        &mut working.1
                    };
                    let name = |kind| {
                        String::from_utf8(
                            string_attribute(attribute(&list, kind).unwrap()).to_vec(),
                        )
                        .unwrap()
                    };
                    let result: std::result::Result<String, i32> = match header.message_type & 0xff
                    {
                        NFT_MSG_DELRULE => {
                            let chain = name(NFTA_RULE_CHAIN);
                            match attribute(&list, NFTA_RULE_HANDLE) {
                                Some(value) => {
                                    let handle = u64::from_be_bytes(value.try_into().unwrap());
                                    match table
                                        .rules
                                        .iter()
                                        .position(|(c, h, _)| *c == chain && *h == handle)
                                    {
                                        Some(index) => {
                                            table.rules.remove(index);
                                            Ok(format!("{label} delete rule {chain} handle"))
                                        }
                                        None => Err(libc::ENOENT),
                                    }
                                }
                                None => {
                                    table.rules.retain(|(c, _, _)| *c != chain);
                                    Ok(format!("{label} flush chain {chain}"))
                                }
                            }
                        }
                        NFT_MSG_DELCHAIN => {
                            let chain = name(NFTA_CHAIN_NAME);
                            let referenced = table
                                .rules
                                .iter()
                                .any(|(_, _, r)| matches!(r, Rule::Jump(t) if *t == chain));
                            match table.chains.iter().position(|c| *c == chain) {
                                Some(_) if referenced => Err(libc::EBUSY),
                                Some(index) => {
                                    table.chains.remove(index);
                                    Ok(format!("{label} delete chain {chain}"))
                                }
                                None => Err(libc::ENOENT),
                            }
                        }
                        _ => Err(libc::EINVAL),
                    };
                    match result {
                        Ok(op) => applied.push(op),
                        Err(errno) => {
                            failure = Some(error(*header, errno));
                            break;
                        }
                    }
                }
                match failure {
                    Some(reply) => {
                        kernel.refused += 1;
                        kernel.pending.push_back(reply);
                    }
                    None => {
                        kernel.ipv4 = working.0;
                        kernel.ipv6 = working.1;
                        kernel.udp_ipv4 = working.2;
                        kernel.udp_ipv6 = working.3;
                        kernel.commits.push(applied);
                        kernel.genid += 1;
                        let (end, _) = messages.last().unwrap();
                        kernel.pending.push_back(error(*end, 0));
                    }
                }
                return Ok(());
            }
            let family = messages[0].1[0];
            let list = attributes(&messages[0].1[4..])?;
            let mut reply = Vec::new();
            if first.message_type & 0xff == NFT_MSG_GETGEN {
                let mut attrs = Vec::new();
                append_attribute(&mut attrs, NFTA_GEN_ID, &kernel.genid.to_be_bytes());
                kernel.pending.push_back(message(
                    nft_type(NFT_MSG_NEWGEN),
                    0,
                    first.sequence,
                    0,
                    0,
                    &attrs,
                ));
                return Ok(());
            }
            match first.message_type & 0xff {
                NFT_MSG_GETCHAIN => {
                    let chains = kernel.table(family).chains.clone();
                    for chain in chains {
                        let mut attrs = Vec::new();
                        append_string(&mut attrs, NFTA_CHAIN_TABLE, FILTER_TABLE);
                        append_string(&mut attrs, NFTA_CHAIN_NAME, chain);
                        reply.extend(message(
                            nft_type(NFT_MSG_NEWCHAIN),
                            0x2,
                            first.sequence,
                            family,
                            0,
                            &attrs,
                        ));
                    }
                }
                NFT_MSG_GETRULE => {
                    let wanted = String::from_utf8(
                        string_attribute(attribute(&list, NFTA_RULE_CHAIN).unwrap()).to_vec(),
                    )
                    .unwrap();
                    let rules = kernel.table(family).rules.clone();
                    for (chain, handle, rule) in rules.iter().filter(|(c, _, _)| *c == wanted) {
                        reply.extend(rule_record(family, chain, *handle, rule, first.sequence));
                    }
                }
                other => bail!("model kernel: unexpected request {other}"),
            }
            reply.extend(done(first.sequence));
            kernel.pending.push_back(reply);
            Ok(())
        }

        fn receive(&mut self) -> Result<&[u8]> {
            let next = self.kernel.lock().unwrap().pending.pop_front();
            match next {
                Some(datagram) => {
                    self.buffer = datagram;
                    Ok(&self.buffer)
                }
                None => bail!("model kernel: receive with nothing queued"),
            }
        }

        fn reserve_receive_buffer(&mut self, bytes: usize) -> Result<usize> {
            Ok(bytes)
        }
    }
}
