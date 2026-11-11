import Foundation
import Testing
import WonderPairing
@testable import WonderVoice

/// Live check against a development wonderd that holds an OpenAI key: mints a
/// client secret, opens a Realtime session, asks by text what projects there
/// are and expects the answer to come through the list_projects tool. Runs
/// only with WONDER_VOICE_LIVE_ORIGIN and WONDER_VOICE_LIVE_CAPABILITY set; it
/// spends a few cents of the key owner's OpenAI usage.
@Suite struct VoiceLiveTests {
    struct LoopbackHost: VoiceHost {
        let origin: URL
        let capability: String
        func get<T: Decodable>(_ path: String, as: T.Type) async throws -> T {
            var request = URLRequest(url: origin.appendingPathComponent(path))
            request.setValue(capability, forHTTPHeaderField: "x-wonder-loopback-capability")
            let (data, response) = try await URLSession.shared.data(for: request)
            guard (response as? HTTPURLResponse)?.statusCode == 200 else {
                throw PairingFailure.response((response as? HTTPURLResponse)?.statusCode ?? 0)
            }
            return try JSONDecoder().decode(T.self, from: data)
        }
        func projects() async throws -> [VoiceProject] {
            try await get("api/v1/projects", as: ProjectsResponse.self).projects.map { VoiceProject(id: $0.id, name: $0.name) }
        }
        func threads(projectID: String?) async throws -> [VoiceThread] {
            guard let projectID else { return [] }
            return try await get("api/v1/projects/\(projectID)/threads", as: ProjectThreadsPage.self).threads.map {
                VoiceThread(id: $0.conversationId ?? $0.reference, title: $0.title, provider: $0.family, isWorking: $0.isWorking)
            }
        }
        func latestReply(threadID: String) async throws -> VoiceReply? { nil }
        func send(threadID: String, text: String) async throws { Issue.record("live check must not send work") }
        func startThread(projectID: String, provider: AgentFamily, model: String?, text: String) async throws -> VoiceThread {
            Issue.record("live check must not start work"); throw PairingFailure.response(403)
        }
        func runningWork() async throws -> [VoiceThread] { [] }
        func stopReply(threadID: String) async throws -> Bool { false }
    }

    final class Collected: @unchecked Sendable {
        private let lock = NSLock()
        private var events: [VoiceRealtimeEvent] = []
        func append(_ event: VoiceRealtimeEvent) { lock.withLock { events.append(event) } }
        var values: [VoiceRealtimeEvent] { lock.withLock { events } }
    }

    @Test(.enabled(if: ProcessInfo.processInfo.environment["WONDER_VOICE_LIVE_ORIGIN"] != nil))
    func projectsQuestionRoundTripsThroughTheTool() async throws {
        let environment = ProcessInfo.processInfo.environment
        let origin = try #require(URL(string: environment["WONDER_VOICE_LIVE_ORIGIN"] ?? ""))
        let capability = try #require(environment["WONDER_VOICE_LIVE_CAPABILITY"])
        var mint = URLRequest(url: origin.appendingPathComponent("api/v1/voice/sessions"))
        mint.httpMethod = "POST"
        mint.setValue(capability, forHTTPHeaderField: "x-wonder-loopback-capability")
        let (data, response) = try await URLSession.shared.data(for: mint)
        #expect((response as? HTTPURLResponse)?.statusCode == 200)
        let grant = try JSONDecoder().decode(VoiceSessionGrant.self, from: data)
        #expect(grant.clientSecret.hasPrefix("ek_"))
        #expect(!String(decoding: data, as: UTF8.self).contains("sk-"), "the mint response never carries the key")

        let host = LoopbackHost(origin: origin, capability: capability)
        let expected = try await host.projects().map(\.name)
        let router = VoiceToolRouter(host: host, context: VoiceContext())
        let collected = Collected()
        let connection = VoiceRealtimeConnection(grant: grant, router: router,
            instructions: VoiceToolRouter.instructions(context: VoiceContext())) { collected.append($0) }
        try await connection.start()
        try await connection.sendText("What projects do I have?")
        let deadline = Date().addingTimeInterval(45)
        func finalAnswer() -> String? {
            let values = collected.values
            guard let tool = values.firstIndex(of: .toolFinished("list_projects")) else { return nil }
            for case .assistantTranscript(let text, true) in values[tool...] { return text }
            return nil
        }
        while finalAnswer() == nil, Date() < deadline {
            if case .closed(let reason)? = collected.values.last { Issue.record("closed: \(reason ?? "normal")"); break }
            try await Task.sleep(for: .milliseconds(200))
        }
        await connection.close()
        let answer = try #require(finalAnswer(), "events: \(collected.values.filter { if case .assistantAudio = $0 { false } else { true } })")
        let audioBytes = collected.values.reduce(0) { if case .assistantAudio(let pcm) = $1 { $0 + pcm.count } else { $0 } }
        print("VOICE-LIVE answer: \(answer)")
        print("VOICE-LIVE projects: \(expected) audioBytes: \(audioBytes)")
        #expect(audioBytes > 0, "the reply was spoken")
        for name in expected { #expect(answer.localizedCaseInsensitiveContains(name)) }
    }
}
