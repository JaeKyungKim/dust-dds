use super::error::RtpsError;
use crate::{
    rtps_messages::{
        self,
        submessage_elements::{Parameter, ParameterList, SerializedDataFragment},
        submessages::{data::DataSubmessage, data_frag::DataFragSubmessage},
        types::ParameterId,
    },
    transport::types::{CacheChange, ChangeKind, EntityId, Guid, GuidPrefix, SampleIdentity},
};
use alloc::{sync::Arc, vec::Vec};

pub const PID_KEY_HASH: ParameterId = 0x0070;
pub const PID_STATUS_INFO: ParameterId = 0x0071;
pub const PID_RELATED_SAMPLE_IDENTITY: ParameterId = 0x0083;
/// eProsima Fast DDS 2.6.x vendor extension for RELATED_SAMPLE_IDENTITY.
///
/// Humble `rmw_fastrtps_cpp` uses this PID exclusively for DDS-RPC
/// reply correlation (see Fast DDS 2.6.x
/// `include/fastdds/dds/core/policy/ParameterTypes.hpp:152` and
/// `src/cpp/fastdds/core/policy/ParameterList.cpp:64-76` —
/// `updateCacheChangeFromInlineQos` recognizes 0x800f and falls
/// through `default:` for the standards-compliant 0x0083).
///
/// We emit both PIDs on writes and accept either on reads so we
/// interop with Humble Fast DDS while staying spec-correct for
/// Fast DDS ≥3.x, Cyclone (when it adopts 0x0083), and any other
/// compliant peer. The payload layout is identical — only the
/// parameter id number differs.
pub const PID_RELATED_SAMPLE_IDENTITY_VENDOR_FASTDDS: ParameterId = 0x800fu16 as ParameterId;

/// Serialised length of a SampleIdentity parameter payload per
/// RTPS 2.3 §9.6.2.9: GuidPrefix (12 octets) + EntityId (4 octets) +
/// SequenceNumber (high: i32 + low: u32, 4 + 4 octets) = 24 octets.
const SAMPLE_IDENTITY_WIRE_SIZE: usize = 24;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct StatusInfo(pub [u8; 4]);
const STATUS_INFO_DISPOSED: StatusInfo = StatusInfo([0, 0, 0, 0b00000001]);
const STATUS_INFO_UNREGISTERED: StatusInfo = StatusInfo([0, 0, 0, 0b0000010]);
const STATUS_INFO_DISPOSED_UNREGISTERED: StatusInfo = StatusInfo([0, 0, 0, 0b00000011]);
const STATUS_INFO_FILTERED: StatusInfo = StatusInfo([0, 0, 0, 0b0000100]);

/// Encode a SampleIdentity as a 24-octet little-endian payload suitable
/// for the RELATED_SAMPLE_IDENTITY inline QoS parameter.
///
/// Layout (RTPS 2.3 §9.6.2.9 + §9.3.2):
///   bytes [ 0..12] — GuidPrefix (raw, endianness-independent)
///   bytes [12..16] — EntityId   (raw, endianness-independent)
///   bytes [16..20] — SequenceNumber.high (i32, little-endian)
///   bytes [20..24] — SequenceNumber.low  (u32, little-endian)
fn encode_sample_identity_le(id: &SampleIdentity) -> [u8; SAMPLE_IDENTITY_WIRE_SIZE] {
    let mut out = [0u8; SAMPLE_IDENTITY_WIRE_SIZE];
    let guid_bytes: [u8; 16] = id.writer_guid().into();
    out[..16].copy_from_slice(&guid_bytes);
    let seq = id.sequence_number();
    let high = (seq >> 32) as i32;
    let low = (seq & 0xFFFF_FFFF) as u32;
    out[16..20].copy_from_slice(&high.to_le_bytes());
    out[20..24].copy_from_slice(&low.to_le_bytes());
    out
}

/// Decode a SampleIdentity payload produced by `encode_sample_identity_le`.
/// Returns `None` for malformed input (length ≠ 24).
fn decode_sample_identity_le(bytes: &[u8]) -> Option<SampleIdentity> {
    if bytes.len() != SAMPLE_IDENTITY_WIRE_SIZE {
        return None;
    }
    let guid_bytes: [u8; 16] = bytes[..16].try_into().ok()?;
    let high = i32::from_le_bytes(bytes[16..20].try_into().ok()?);
    let low = u32::from_le_bytes(bytes[20..24].try_into().ok()?);
    let seq = ((high as i64) << 32) | (low as i64 & 0xFFFF_FFFF);
    Some(SampleIdentity::new(Guid::from(guid_bytes), seq))
}

impl CacheChange {
    pub fn as_data_submessage(&self, reader_id: EntityId, writer_id: EntityId) -> DataSubmessage {
        let (data_flag, key_flag) = match self.kind {
            ChangeKind::Alive | ChangeKind::AliveFiltered => (true, false),
            ChangeKind::NotAliveDisposed
            | ChangeKind::NotAliveUnregistered
            | ChangeKind::NotAliveDisposedUnregistered => (false, true),
        };

        let mut parameters = Vec::with_capacity(3);
        match self.kind {
            ChangeKind::Alive | ChangeKind::AliveFiltered => (),
            ChangeKind::NotAliveDisposed => parameters.push(Parameter::new(
                PID_STATUS_INFO,
                Arc::from(STATUS_INFO_DISPOSED.0),
            )),
            ChangeKind::NotAliveUnregistered => parameters.push(Parameter::new(
                PID_STATUS_INFO,
                Arc::from(STATUS_INFO_UNREGISTERED.0),
            )),
            ChangeKind::NotAliveDisposedUnregistered => parameters.push(Parameter::new(
                PID_STATUS_INFO,
                Arc::from(STATUS_INFO_DISPOSED_UNREGISTERED.0),
            )),
        }

        if let Some(i) = self.instance_handle {
            parameters.push(Parameter::new(PID_KEY_HASH, Arc::from(i)));
        }
        if let Some(sid) = self.related_sample_identity {
            let payload: Arc<[u8]> = Arc::from(encode_sample_identity_le(&sid));
            // Emit the standard PID first so first-match readers pick
            // the spec-correct value.
            parameters.push(Parameter::new(
                PID_RELATED_SAMPLE_IDENTITY,
                Arc::clone(&payload),
            ));
            // Then the Fast DDS 2.6.x vendor PID so Humble Fast DDS
            // (which ignores 0x0083) also sees the identity.
            parameters.push(Parameter::new(
                PID_RELATED_SAMPLE_IDENTITY_VENDOR_FASTDDS,
                payload,
            ));
        }
        let parameter_list = ParameterList::new(parameters);

        DataSubmessage::new(
            true,
            data_flag,
            key_flag,
            false,
            reader_id,
            writer_id,
            self.sequence_number,
            parameter_list,
            self.data_value.clone().into(),
        )
    }

    pub fn try_from_data_submessage(
        data_submessage: &DataSubmessage,
        source_guid_prefix: GuidPrefix,
        source_timestamp: Option<rtps_messages::types::Time>,
    ) -> Result<Self, RtpsError> {
        let kind = match data_submessage
            .inline_qos()
            .parameter()
            .iter()
            .find(|&x| x.parameter_id() == PID_STATUS_INFO)
        {
            Some(p) => {
                if p.length() == 4 {
                    let status_info =
                        StatusInfo([p.value()[0], p.value()[1], p.value()[2], p.value()[3]]);
                    match status_info {
                        STATUS_INFO_DISPOSED => Ok(ChangeKind::NotAliveDisposed),
                        STATUS_INFO_UNREGISTERED => Ok(ChangeKind::NotAliveUnregistered),
                        STATUS_INFO_DISPOSED_UNREGISTERED => {
                            Ok(ChangeKind::NotAliveDisposedUnregistered)
                        }
                        STATUS_INFO_FILTERED => Ok(ChangeKind::AliveFiltered),
                        _ => Err(RtpsError::InvalidData),
                    }
                } else {
                    Err(RtpsError::InvalidData)
                }
            }
            None => Ok(ChangeKind::Alive),
        }?;

        let instance_handle = match data_submessage
            .inline_qos()
            .parameter()
            .iter()
            .find(|&x| x.parameter_id() == PID_KEY_HASH)
        {
            Some(p) => <[u8; 16]>::try_from(p.value()).ok(),

            None => None,
        };

        // Accept either the standard PID or the Fast DDS 2.6.x vendor
        // PID. first-match: writers that emit both (ours) put the
        // standard PID first, so we end up reading the spec-correct
        // bytes; peers that only use the vendor extension
        // (Humble Fast DDS) still match.
        let related_sample_identity = data_submessage
            .inline_qos()
            .parameter()
            .iter()
            .find(|&x| {
                matches!(
                    x.parameter_id(),
                    PID_RELATED_SAMPLE_IDENTITY | PID_RELATED_SAMPLE_IDENTITY_VENDOR_FASTDDS,
                )
            })
            .and_then(|p| decode_sample_identity_le(p.value()));

        Ok(CacheChange {
            kind,
            writer_guid: Guid::new(source_guid_prefix, data_submessage.writer_id()),
            source_timestamp: source_timestamp.map(Into::into),
            instance_handle,
            sequence_number: data_submessage.writer_sn(),
            data_value: data_submessage.serialized_payload().clone().into(),
            related_sample_identity,
        })
    }

    pub fn as_data_frag_submessage(
        &self,
        reader_id: EntityId,
        writer_id: EntityId,
        data_max_size_serialized: usize,
        fragment_number: usize,
    ) -> DataFragSubmessage {
        let inline_qos_flag = true;
        let key_flag = false;
        let non_standard_payload_flag = false;
        let writer_sn = self.sequence_number;
        let fragment_starting_num = (fragment_number + 1) as u32;
        let fragments_in_submessage = 1;
        let fragment_size = data_max_size_serialized as u16;
        let data_size = self.data_value.len() as u32;

        let start = fragment_number * data_max_size_serialized;
        let end = core::cmp::min(
            (fragment_number + 1) * data_max_size_serialized,
            self.data_value.len(),
        );

        let serialized_payload =
            SerializedDataFragment::new(self.data_value.clone().into(), start..end);

        DataFragSubmessage::new(
            inline_qos_flag,
            non_standard_payload_flag,
            key_flag,
            reader_id,
            writer_id,
            writer_sn,
            fragment_starting_num,
            fragments_in_submessage,
            fragment_size,
            data_size,
            ParameterList::new(Vec::new()),
            serialized_payload,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::types::USER_DEFINED_WRITER_WITH_KEY;

    fn sample_identity_fixture() -> SampleIdentity {
        SampleIdentity::new(
            Guid::new(
                [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
                EntityId::new([0x00, 0x00, 0x12], USER_DEFINED_WRITER_WITH_KEY),
            ),
            0x0000_0001_0000_0002, // high = 1, low = 2
        )
    }

    #[test]
    fn sample_identity_encode_matches_rtps_layout() {
        let id = sample_identity_fixture();
        let bytes = encode_sample_identity_le(&id);
        assert_eq!(bytes.len(), 24);
        // GuidPrefix (raw)
        assert_eq!(&bytes[..12], &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        // EntityId (raw: entity_key + entity_kind)
        assert_eq!(
            &bytes[12..16],
            &[0x00, 0x00, 0x12, USER_DEFINED_WRITER_WITH_KEY]
        );
        // SequenceNumber.high little-endian = 1
        assert_eq!(&bytes[16..20], &[1, 0, 0, 0]);
        // SequenceNumber.low little-endian = 2
        assert_eq!(&bytes[20..24], &[2, 0, 0, 0]);
    }

    #[test]
    fn sample_identity_encode_decode_roundtrip() {
        let id = sample_identity_fixture();
        let bytes = encode_sample_identity_le(&id);
        let decoded = decode_sample_identity_le(&bytes).expect("decode");
        assert_eq!(decoded, id);
    }

    #[test]
    fn sample_identity_decode_rejects_wrong_length() {
        assert!(decode_sample_identity_le(&[0u8; 23]).is_none());
        assert!(decode_sample_identity_le(&[0u8; 25]).is_none());
        assert!(decode_sample_identity_le(&[]).is_none());
    }

    // ── B7 — Fast DDS 2.6.x vendor PID 0x800f dual-emit / dual-accept ──

    use crate::transport::types::{ChangeKind, USER_DEFINED_READER_WITH_KEY};

    fn cache_change_with_identity(sid: SampleIdentity) -> CacheChange {
        CacheChange {
            kind: ChangeKind::Alive,
            writer_guid: Guid::new(
                [0xAA; 12],
                EntityId::new([0x00, 0x00, 0x34], USER_DEFINED_WRITER_WITH_KEY),
            ),
            sequence_number: 42,
            source_timestamp: None,
            instance_handle: None,
            data_value: Arc::from([1u8, 2, 3, 4] as [u8; 4]),
            related_sample_identity: Some(sid),
        }
    }

    fn entity_ids() -> (EntityId, EntityId) {
        (
            EntityId::new([0x00, 0x00, 0x07], USER_DEFINED_READER_WITH_KEY),
            EntityId::new([0x00, 0x00, 0x34], USER_DEFINED_WRITER_WITH_KEY),
        )
    }

    /// B7-T1 — write path emits BOTH the standard 0x0083 and the
    /// Fast DDS 2.6.x vendor 0x800f parameters, with the standard
    /// first so first-match readers get the spec-correct bytes.
    #[test]
    fn reply_carries_both_pids() {
        let sid = sample_identity_fixture();
        let change = cache_change_with_identity(sid);
        let (reader_id, writer_id) = entity_ids();
        let submessage = change.as_data_submessage(reader_id, writer_id);
        let params: Vec<ParameterId> = submessage
            .inline_qos()
            .parameter()
            .iter()
            .map(|p| p.parameter_id())
            .collect();
        assert!(
            params.contains(&PID_RELATED_SAMPLE_IDENTITY),
            "standard 0x0083 missing: {params:?}"
        );
        assert!(
            params.contains(&PID_RELATED_SAMPLE_IDENTITY_VENDOR_FASTDDS),
            "vendor 0x800f missing: {params:?}"
        );
        let std_pos = params
            .iter()
            .position(|&p| p == PID_RELATED_SAMPLE_IDENTITY)
            .unwrap();
        let vendor_pos = params
            .iter()
            .position(|&p| p == PID_RELATED_SAMPLE_IDENTITY_VENDOR_FASTDDS)
            .unwrap();
        assert!(
            std_pos < vendor_pos,
            "standard PID must come before vendor PID for first-match priority: \
             got std at {std_pos}, vendor at {vendor_pos}"
        );
    }

    /// B7-T2 — on the write path, both parameters share byte-identical
    /// 24-byte payloads. A peer that accepts only one of them still
    /// sees the same SampleIdentity.
    #[test]
    fn payload_layout_identical_across_pids() {
        let sid = sample_identity_fixture();
        let change = cache_change_with_identity(sid);
        let (reader_id, writer_id) = entity_ids();
        let submessage = change.as_data_submessage(reader_id, writer_id);
        let mut std_bytes: Option<&[u8]> = None;
        let mut vendor_bytes: Option<&[u8]> = None;
        for p in submessage.inline_qos().parameter().iter() {
            match p.parameter_id() {
                PID_RELATED_SAMPLE_IDENTITY => std_bytes = Some(p.value()),
                PID_RELATED_SAMPLE_IDENTITY_VENDOR_FASTDDS => vendor_bytes = Some(p.value()),
                _ => (),
            }
        }
        let std_bytes = std_bytes.expect("standard PID payload");
        let vendor_bytes = vendor_bytes.expect("vendor PID payload");
        assert_eq!(
            std_bytes, vendor_bytes,
            "dual-emit payloads diverge — identity would be inconsistent across peers"
        );
        assert_eq!(std_bytes.len(), 24, "RTPS §9.3.2 mandates 24-byte layout");
    }

    /// B7-T3 — reader path: peer sent only the standard 0x0083 (e.g.
    /// Fast DDS ≥3.x, future Cyclone). Must still decode correctly.
    #[test]
    fn reader_accepts_standard_pid_only() {
        let sid = sample_identity_fixture();
        let payload: Arc<[u8]> = Arc::from(encode_sample_identity_le(&sid));
        let parameters =
            ParameterList::new(vec![Parameter::new(PID_RELATED_SAMPLE_IDENTITY, payload)]);
        let submessage = make_data_submessage_with_inline_qos(parameters);
        let decoded =
            CacheChange::try_from_data_submessage(&submessage, [0xAA; 12], None).expect("decode");
        assert_eq!(decoded.related_sample_identity, Some(sid));
    }

    /// B7-T4 — reader path: peer sent only the Fast DDS vendor 0x800f
    /// (Humble `rmw_fastrtps_cpp` 2.6.x). Must also decode correctly.
    #[test]
    fn reader_accepts_vendor_pid_only() {
        let sid = sample_identity_fixture();
        let payload: Arc<[u8]> = Arc::from(encode_sample_identity_le(&sid));
        let parameters = ParameterList::new(vec![Parameter::new(
            PID_RELATED_SAMPLE_IDENTITY_VENDOR_FASTDDS,
            payload,
        )]);
        let submessage = make_data_submessage_with_inline_qos(parameters);
        let decoded =
            CacheChange::try_from_data_submessage(&submessage, [0xAA; 12], None).expect("decode");
        assert_eq!(decoded.related_sample_identity, Some(sid));
    }

    /// B7-T5 — reader path: peer sent both PIDs (e.g. our own writer,
    /// or a peer doing the same dual-emit). First-match wins and
    /// produces the same identity either way since the payloads are
    /// identical (T2).
    #[test]
    fn reader_prefers_first_match_when_both_present() {
        let sid = sample_identity_fixture();
        let payload: Arc<[u8]> = Arc::from(encode_sample_identity_le(&sid));
        let parameters = ParameterList::new(vec![
            Parameter::new(PID_RELATED_SAMPLE_IDENTITY, Arc::clone(&payload)),
            Parameter::new(PID_RELATED_SAMPLE_IDENTITY_VENDOR_FASTDDS, payload),
        ]);
        let submessage = make_data_submessage_with_inline_qos(parameters);
        let decoded =
            CacheChange::try_from_data_submessage(&submessage, [0xAA; 12], None).expect("decode");
        assert_eq!(decoded.related_sample_identity, Some(sid));
    }

    fn make_data_submessage_with_inline_qos(parameters: ParameterList) -> DataSubmessage {
        let (reader_id, writer_id) = entity_ids();
        DataSubmessage::new(
            true,
            true,
            false,
            false,
            reader_id,
            writer_id,
            7,
            parameters,
            Arc::<[u8]>::from([].as_slice()).into(),
        )
    }
}
