import Foundation
import Testing
@testable import WonderVoice

private func fixture() throws -> [String: Any] {
    let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    let data = try Data(contentsOf: root.appendingPathComponent("packages/protocol/fixtures/voice-v1.json"))
    return try #require(try JSONSerialization.jsonObject(with: data) as? [String: Any])
}

private func json(_ value: Any) throws -> Data { try JSONSerialization.data(withJSONObject: value) }
private func object(_ data: Data) -> [String: Any] {
    (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
}

@Suite struct VoiceSessionTests {
    /// The Mac's mint and settings replies decode into what the phone uses.
    @Test func macRepliesDecode() throws {
        let fixture = try fixture()
        let grant = try JSONDecoder().decode(VoiceSessionGrant.self, from: json(fixture["voiceSession"]!))
        #expect(grant.panelId == "panel-3F2A9C41" && grant.sessionLimitSeconds == 900)
        #expect(grant.sessionEndsAt == 1_792_000_900 && grant.sessionEndsFor == "sessionLimit")
        let limits = try JSONDecoder().decode(VoiceLimits.self, from: json(fixture["voiceSettings"]!))
        #expect(limits.sessionLimitMinutes == 15 && limits.sessionLimitMaxMinutes == 55 && limits.dailyMinutes == 60)
        #expect(limits.dailyDollars == nil && limits.keepTranscripts && limits.today?.spokenSeconds == 312)
        // An older Mac's mint has no limit fields.
        let old = try JSONDecoder().decode(VoiceSessionGrant.self, from: json(["provider": "openai", "clientSecret": "ek_1",
            "expiresAt": 1, "model": "m", "url": "wss://api.openai.com/v1/realtime"]))
        #expect(old.sessionEndsAt == nil)
    }

    @Test func panelIDsAreWhatTheMacAccepts() {
        let id = makeVoicePanelID()
        #expect((8...64).contains(id.count))
        #expect(id.allSatisfy { $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "-" || $0 == "_") })
        #expect(id != makeVoicePanelID())
    }

    /// The phone ends first: before the Mac's end (session or daily limit) and
    /// before OpenAI's one-hour cap on the connection.
    @Test(arguments: [
        // (seconds from connect to the Mac's end, endsFor, expected reason)
        (900, "sessionLimit", VoiceSessionLimit.Reason.sessionLimit(minutes: 15)),
        (120, "dailyLimit", .dailyLimit),
        (3_300, "sessionLimit", .sessionLimit(minutes: 15)),
        (3_600, "sessionLimit", .connectionCap),
        (7_200, "sessionLimit", .connectionCap),
    ])
    func deadlineIsTheEarliestLimitLessAMargin(macEnd: Int, endsFor: String, reason: VoiceSessionLimit.Reason) {
        let connected = Date(timeIntervalSince1970: 1_000_000)
        let grant = VoiceSessionGrant(clientSecret: "ek_1", expiresAt: 0, model: "m", url: "", panelId: "panel-1",
                                      sessionLimitSeconds: 900, sessionEndsAt: 1_000_000 + macEnd, sessionEndsFor: endsFor)
        let (date, found) = VoiceSessionLimit.deadline(grant: grant, connectedAt: connected)
        #expect(found == reason)
        let expected = min(TimeInterval(macEnd), VoiceSessionLimit.connectionCap) - VoiceSessionLimit.margin
        #expect(date.timeIntervalSince(connected) == expected)
        #expect(date.timeIntervalSince(connected) < TimeInterval(macEnd))
        #expect(date.timeIntervalSince(connected) < VoiceSessionLimit.connectionCap)
    }

    @Test func anOlderMacStillEndsBeforeOpenAIsCap() {
        let connected = Date(timeIntervalSince1970: 50)
        let grant = VoiceSessionGrant(clientSecret: "ek_1", expiresAt: 0, model: "m", url: "")
        let (date, reason) = VoiceSessionLimit.deadline(grant: grant, connectedAt: connected)
        #expect(reason == .connectionCap && date < connected.addingTimeInterval(VoiceSessionLimit.connectionCap))
        #expect(!VoiceSessionLimit.panelEnded(.connectionCap) && VoiceSessionLimit.panelEnded(.dailyLimit))
    }

    /// Reports carry exactly the Mac's field names (`deny_unknown_fields`
    /// there), each usage event once, and only new transcript lines.
    @Test func reportsMatchTheMacsShapeAndSendEachThingOnce() throws {
        let fixture = try fixture()
        var log = VoiceSessionLog(prefix: "s1")
        let start = Date(timeIntervalSince1970: 1_792_000_000)
        log.connected(at: start)
        let usage = #"{"input_tokens":1500,"output_tokens":750,"input_token_details":{"text_tokens":1000,"audio_tokens":500}}"#
        for _ in 0..<2 { log.record(.usage(source: "response", id: "resp_1", json: usage), at: start) }
        log.record(.userTranscript("Tell the release", final: false), at: start)
        log.record(.userTranscript("Tell the release chat the build is green.", final: true), at: start.addingTimeInterval(10))
        log.record(.assistantTranscript("  ", final: true), at: start)
        log.record(.assistantTranscript("Sent to Release checklist.", final: true), at: start.addingTimeInterval(14))

        let (usageBody, _) = log.takeUsage(at: start.addingTimeInterval(95))
        let report = object(usageBody)
        let expected = try #require(fixture["voiceUsageReport"] as? [String: Any])
        #expect(Set(report.keys) == Set(expected.keys))
        #expect(report["connectedSeconds"] as? Int == 95)
        let events = try #require(report["events"] as? [[String: Any]])
        #expect(events.count == 1 && Set(events[0].keys) == Set(((expected["events"] as? [[String: Any]])?[0] ?? [:]).keys))
        #expect((events[0]["usage"] as? [String: Any])?["input_tokens"] as? Int == 1500)
        #expect((object(log.takeUsage(at: start).body)["events"] as? [Any])?.isEmpty == true, "an event goes once")

        let (body, sent) = log.takeTranscript(threadIDs: ["conversation-release"])
        let transcript = object(body)
        let expectedTranscript = try #require(fixture["voiceTranscriptReport"] as? [String: Any])
        #expect(Set(transcript.keys) == Set(expectedTranscript.keys))
        let entries = try #require(transcript["entries"] as? [[String: Any]])
        let expectedEntry = try #require((expectedTranscript["entries"] as? [[String: Any]])?.first)
        #expect(entries.count == 2 && Set(entries[0].keys) == Set(expectedEntry.keys))
        #expect(entries.map { $0["speaker"] as? String } == ["owner", "voice"])
        #expect(entries[0]["atMs"] as? Int == 1_792_000_010_000 && entries[0]["id"] as? String == "s1-1")
        #expect(!log.hasUnsentTranscript)
        // A failed post puts the lines back, in order, with the same IDs.
        log.restore(sent)
        log.record(.userTranscript("Thanks", final: true), at: start)
        let retried = (object(log.takeTranscript(threadIDs: []).body)["entries"] as? [[String: Any]])?.map { $0["id"] as? String }
        #expect(retried == ["s1-1", "s1-2", "s1-3"])
    }

    @Test func connectedTimeAddsUpAcrossReconnects() {
        var log = VoiceSessionLog()
        let start = Date(timeIntervalSince1970: 0)
        log.connected(at: start)
        log.disconnected(at: start.addingTimeInterval(30))
        #expect(log.connectedSeconds(at: start.addingTimeInterval(100)) == 30)
        log.connected(at: start.addingTimeInterval(100))
        log.connected(at: start.addingTimeInterval(110))
        #expect(log.connectedSeconds(at: start.addingTimeInterval(120)) == 50)
    }
}
