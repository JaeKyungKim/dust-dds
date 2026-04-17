use dust_dds::{
    domain::domain_participant_factory::DomainParticipantFactory,
    infrastructure::{
        instance::WriteParams,
        listener::NO_LISTENER,
        qos::{DataReaderQos, DataWriterQos, QosKind},
        qos_policy::{ReliabilityQosPolicy, ReliabilityQosPolicyKind},
        sample_info::{ANY_INSTANCE_STATE, ANY_SAMPLE_STATE, ANY_VIEW_STATE},
        status::{NO_STATUS, StatusKind},
        time::{Duration, DurationKind},
        type_support::{DdsType, TypeSupport},
    },
    wait_set::{Condition, WaitSet},
};

mod utils;
use crate::utils::domain_id_generator::TEST_DOMAIN_ID_GENERATOR;

#[derive(Debug, PartialEq, DdsType)]
struct TestMessage(u32);

/// Build a participant/topic/writer/reader quad with reliable QoS and
/// wait until the writer sees a matched subscription.
fn reliable_writer_reader_pair() -> (
    dust_dds::publication::data_writer::DataWriter<TestMessage>,
    dust_dds::subscription::data_reader::DataReader<TestMessage>,
) {
    let domain_id = TEST_DOMAIN_ID_GENERATOR.generate_unique_domain_id();
    let participant = DomainParticipantFactory::get_instance()
        .create_participant(domain_id, QosKind::Default, NO_LISTENER, NO_STATUS)
        .unwrap();

    let topic = participant
        .create_topic::<TestMessage>(
            "RelatedIdentityTopic",
            "TestMessage",
            QosKind::Default,
            NO_LISTENER,
            NO_STATUS,
        )
        .unwrap();

    let publisher = participant
        .create_publisher(QosKind::Default, NO_LISTENER, NO_STATUS)
        .unwrap();
    let writer_qos = DataWriterQos {
        reliability: ReliabilityQosPolicy {
            kind: ReliabilityQosPolicyKind::Reliable,
            max_blocking_time: DurationKind::Finite(Duration::new(1, 0)),
        },
        ..Default::default()
    };
    let writer = publisher
        .create_datawriter(
            &topic,
            QosKind::Specific(writer_qos),
            NO_LISTENER,
            NO_STATUS,
        )
        .unwrap();

    let subscriber = participant
        .create_subscriber(QosKind::Default, NO_LISTENER, NO_STATUS)
        .unwrap();
    let reader_qos = DataReaderQos {
        reliability: ReliabilityQosPolicy {
            kind: ReliabilityQosPolicyKind::Reliable,
            max_blocking_time: DurationKind::Finite(Duration::new(1, 0)),
        },
        ..Default::default()
    };
    let reader = subscriber
        .create_datareader::<TestMessage>(
            &topic,
            QosKind::Specific(reader_qos),
            NO_LISTENER,
            NO_STATUS,
        )
        .unwrap();

    let cond = writer.get_statuscondition();
    cond.set_enabled_statuses(&[StatusKind::PublicationMatched])
        .unwrap();
    let mut ws = WaitSet::new();
    ws.attach_condition(Condition::StatusCondition(cond))
        .unwrap();
    ws.wait(Duration::new(10, 0)).unwrap();

    (writer, reader)
}

/// Wait for one sample to arrive at the reader. Panics on timeout.
fn wait_for_sample<T: TypeSupport>(reader: &dust_dds::subscription::data_reader::DataReader<T>) {
    let cond = reader.get_statuscondition();
    cond.set_enabled_statuses(&[StatusKind::DataAvailable])
        .unwrap();
    let mut ws = WaitSet::new();
    ws.attach_condition(Condition::StatusCondition(cond))
        .unwrap();
    ws.wait(Duration::new(10, 0)).unwrap();
}

/// End-to-end verification that a writer-attached `related_sample_identity`
/// survives the RTPS PID 0x0083 encode/decode path and surfaces as
/// `SampleInfo::related_sample_identity` on the reader side.
#[test]
fn write_w_params_propagates_related_sample_identity_end_to_end() {
    let (writer, reader) = reliable_writer_reader_pair();

    // Step 1 — plain write. The reader uses this sample's SampleInfo to
    // obtain an identity we will reference on the next write.
    writer.write(TestMessage(1), None).unwrap();
    wait_for_sample(&reader);
    let first = reader
        .take(1, ANY_SAMPLE_STATE, ANY_VIEW_STATE, ANY_INSTANCE_STATE)
        .unwrap();
    assert_eq!(first.len(), 1);
    let first_identity = first[0]
        .sample_info
        .sample_identity
        .expect("first sample must surface a sample_identity");
    assert!(
        first[0].sample_info.related_sample_identity.is_none(),
        "plain write must not attach RELATED_SAMPLE_IDENTITY",
    );

    // Step 2 — write again with the first sample's identity set as
    // related_sample_identity. The reader must see it on SampleInfo.
    let returned = writer
        .write_w_params(
            TestMessage(2),
            WriteParams::default().with_related_sample_identity(first_identity),
        )
        .unwrap();
    wait_for_sample(&reader);
    let second = reader
        .take(1, ANY_SAMPLE_STATE, ANY_VIEW_STATE, ANY_INSTANCE_STATE)
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(
        second[0].sample_info.related_sample_identity,
        Some(first_identity),
        "PID 0x0083 must round-trip end-to-end",
    );

    // The identity returned by write_w_params must match the one that
    // shows up on the reader side as sample_identity.
    assert_eq!(
        second[0].sample_info.sample_identity,
        Some(returned),
        "write_w_params return value must match receiver's sample_identity",
    );
}

/// A default `write()` should populate `sample_identity` on the reader
/// side but leave `related_sample_identity` as `None`. Guards against a
/// regression where the encoder accidentally emits PID 0x0083 for every
/// sample.
#[test]
fn default_write_exposes_sample_identity_without_related() {
    let (writer, reader) = reliable_writer_reader_pair();

    writer.write(TestMessage(7), None).unwrap();
    wait_for_sample(&reader);
    let samples = reader
        .take(1, ANY_SAMPLE_STATE, ANY_VIEW_STATE, ANY_INSTANCE_STATE)
        .unwrap();
    assert_eq!(samples.len(), 1);
    assert!(samples[0].sample_info.sample_identity.is_some());
    assert!(samples[0].sample_info.related_sample_identity.is_none());
}

/// `write_w_params` must return the identity the middleware assigned to
/// the sample. Consecutive writes from the same writer must produce a
/// monotonically increasing sequence_number while keeping the same
/// writer_guid.
#[test]
fn write_w_params_returns_monotonic_identity_from_same_writer() {
    let (writer, _reader) = reliable_writer_reader_pair();

    let id1 = writer
        .write_w_params(TestMessage(10), WriteParams::default())
        .unwrap();
    let id2 = writer
        .write_w_params(TestMessage(11), WriteParams::default())
        .unwrap();
    let id3 = writer
        .write_w_params(TestMessage(12), WriteParams::default())
        .unwrap();

    assert_eq!(id1.writer_guid(), id2.writer_guid());
    assert_eq!(id2.writer_guid(), id3.writer_guid());
    assert!(id1.sequence_number() < id2.sequence_number());
    assert!(id2.sequence_number() < id3.sequence_number());
}
