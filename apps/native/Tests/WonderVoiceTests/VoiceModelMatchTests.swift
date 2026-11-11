import Testing
@testable import WonderVoice

struct VoiceModelMatchTests {
    private let models = [
        VoiceModelMatch.Candidate(id: "deepseek-v4.1-flash-free", name: "DeepSeek V4.1 Flash (free)"),
        VoiceModelMatch.Candidate(id: "deepseek-v4.1-pro", name: "DeepSeek V4.1 Pro"),
        VoiceModelMatch.Candidate(id: "beam-1", name: "Beam"),
    ]

    @Test func spokenNamesFindTheirModel() {
        #expect(VoiceModelMatch.resolve("DeepSeek Flash", among: models) == .match(models[0]))
        #expect(VoiceModelMatch.resolve("deepseek v4.1 pro", among: models) == .match(models[1]))
        #expect(VoiceModelMatch.resolve("beam", among: models) == .match(models[2]))
    }

    @Test func unknownAndVagueNamesAreNotGuessed() {
        #expect(VoiceModelMatch.resolve("GPT Turbo", among: models) == .none)
        #expect(VoiceModelMatch.resolve("DeepSeek", among: models) == .ambiguous([models[0], models[1]]))
    }
}
