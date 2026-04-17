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
            let payload = encode_sample_identity_le(&sid);
            parameters.push(Parameter::new(
                PID_RELATED_SAMPLE_IDENTITY,
                Arc::from(payload),
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

        let related_sample_identity = data_submessage
            .inline_qos()
            .parameter()
            .iter()
            .find(|&x| x.parameter_id() == PID_RELATED_SAMPLE_IDENTITY)
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
}
