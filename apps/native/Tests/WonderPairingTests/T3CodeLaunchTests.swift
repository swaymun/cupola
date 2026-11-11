import XCTest
@testable import WonderPairing

private final class T3CodeLaunchRefusalProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "t3-launch-test.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let words = "Connect T3 Code in Settings to start threads there."
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: 409,
            httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "text/plain"])!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(words.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

// Contract (owner, Oct 20): New chat can start a thread in T3 Code on the
// computer instead of in Cupola. It is offered only when that computer is
// connected to T3 Code and the Project has a folder; the launch carries the
// typed words and the title the Mac would give the thread; a refusal shows the
// Mac's reason and a lost answer says to check T3 Code, never retrying.
final class T3CodeLaunchTests: XCTestCase {
    private func project(folders: Int = 1) -> ProjectSummary {
        ProjectSummary(id: "p", name: "Release", folders: (0..<folders).map {
            ProjectFolder(id: "f\($0)", path: "/work/\($0)", name: "f\($0)", isPrimary: $0 == 0, isAvailable: true)
        })
    }

    private func status(_ json: String) -> T3CodeStatus {
        try! JSONDecoder().decode(T3CodeStatus.self, from: Data(json.utf8))
    }

    func testComputerDefaultAndDraftOverrideSurviveRestorationIndependently() throws {
        let connected = status(#"{"state":"connected","launchProviders":["claude","codex"]}"#)
        let disconnected = status(#"{"state":"disconnected","launchProviders":["claude","codex"]}"#)
        let preferencesData = Data(#"{"families":[],"startsInT3Code":true}"#.utf8)
        let preferences = try JSONDecoder().decode(DefaultModelPreferences.self, from: preferencesData)
        var draft = NewChatDraft(destination: .project(id: "p"), family: .claude)
        func usesT3(_ draft: NewChatDraft, _ status: T3CodeStatus?) -> Bool {
            draft.usesT3Code(defaultEnabled: preferences.startsInT3Code == true, status: status, project: project())
        }
        XCTAssertTrue(usesT3(draft, connected))
        XCTAssertFalse(usesT3(draft, disconnected), "unavailable default falls back to the normal provider")
        XCTAssertFalse(usesT3(draft, nil), "unknown availability never starts a T3 send")
        XCTAssertFalse(draft.usesT3Code(defaultEnabled: false, status: connected, project: project()))
        XCTAssertFalse(draft.usesT3Code(defaultEnabled: true, status: connected, project: project(folders: 0)))
        draft.family = .opencode
        XCTAssertFalse(usesT3(draft, connected))
        draft.family = .claude
        var frozenNormal = NewChatDraft(destination: .project(id: "p"), text: "Already queued", family: .claude)
        frozenNormal.freeze()
        XCTAssertFalse(usesT3(frozenNormal, connected), "a frozen normal request cannot change launch paths when the default changes")
        frozenNormal.startsInT3Code = true
        XCTAssertTrue(usesT3(frozenNormal, disconnected), "an explicitly frozen T3 request still waits for T3")
        draft.startsInT3Code = false
        let restored = try JSONDecoder().decode(NewChatDraft.self, from: JSONEncoder().encode(draft))
        XCTAssertFalse(usesT3(restored, connected), "a per-draft off choice survives relaunch")
        let reloaded = try JSONDecoder().decode(DefaultModelPreferences.self, from: preferencesData)
        XCTAssertEqual(reloaded.startsInT3Code, true, "the draft override leaves the computer default alone")
        draft.startsInT3Code = true
        XCTAssertTrue(usesT3(draft, disconnected), "an explicit T3 choice waits instead of silently falling back")
        XCTAssertNotNil(T3CodeLaunch.blockedReason(draft, status: disconnected, project: project(), connected: true, macName: "Work computer"))
        draft.completeSubmission()
        XCTAssertNil(draft.startsInT3Code)
        XCTAssertTrue(usesT3(draft, connected), "the next draft follows the saved computer default again")
    }

    func testPlanModeStartsT3CodeInPlanModeOnlyWhereItIsTheToggle() throws {
        var draft = NewChatDraft(destination: .project(id: "p"), text: "Plan the release")
        draft.planMode = true
        let planned = T3CodeLaunch.request(for: draft, body: draft.text, project: project(folders: 2), planToggle: true)
        let sent = try JSONSerialization.jsonObject(with: planned.body()) as! [String: Any]
        XCTAssertEqual(sent["planMode"] as? Bool, true)
        XCTAssertEqual(sent["folderId"] as? String, "f0", "the Project's primary folder when none was chosen")
        let other = T3CodeLaunch.request(for: draft, body: draft.text, project: project(), planToggle: false)
        let otherSent = try JSONSerialization.jsonObject(with: other.body()) as! [String: Any]
        XCTAssertNil(otherSent["planMode"], "a provider whose Plan is an access mode sends no plan mode")
        draft.planMode = nil
        let plain = try JSONSerialization.jsonObject(with: T3CodeLaunch.request(for: draft, body: draft.text, project: project(), planToggle: true).body()) as! [String: Any]
        XCTAssertNil(plain["planMode"], "older Macs refuse fields they don't know, so it is omitted when off")
    }

    func testTheRequestCarriesTheWordsFolderAndTheMacsTitle() throws {
        let request = T3CodeLaunchRequest(message: "\n  Draft the release notes  \nfrom the changelog", folderId: "f1")
        let sent = try JSONSerialization.jsonObject(with: request.body()) as! [String: String]
        XCTAssertEqual(sent, ["title": "Draft the release notes", "message": "\n  Draft the release notes  \nfrom the changelog",
                              "folderId": "f1"])
        XCTAssertEqual(T3CodeLaunchRequest.title(for: String(repeating: "a", count: 120)).count, 80)
        XCTAssertLessThanOrEqual(T3CodeLaunchRequest.title(for: String(repeating: "🚀", count: 80)).utf8.count, 200,
                                 "the Mac takes a title of up to 200 bytes")
        XCTAssertEqual(T3CodeLaunchRequest.title(for: "   "), "New thread")
    }

    // Fix round 2 (lead): a draft that chose T3 Code only ever starts there.
    // While T3 Code is unknown, reloading, disconnected or the Mac is offline,
    // Send waits with one line saying why; it never falls back to Cupola.
    func testAChosenT3DraftWaitsInsteadOfStartingInCupola() {
        var draft = NewChatDraft(destination: .project(id: "p"), text: "Draft them", family: .claude)
        draft.startsInT3Code = true
        let cases: [(T3CodeStatus?, ProjectSummary?, Bool, String)] = [
            (status(#"{"state":"connected","launches":true}"#), project(), false, "Can’t reach Studio"),
            (nil, project(), true, "Checking T3 Code"),
            (status(#"{"state":"disconnected","launches":true}"#), project(), true, "isn’t connected"),
            (status(#"{"state":"expired","launches":true}"#), project(), true, "authorization expired"),
            (T3CodeStatus(state: "unavailable"), project(), true, "isn’t reachable"),
            (status(#"{"state":"connected","launches":true}"#), project(folders: 0), true, "no folder"),
        ]
        for (status, project, connected, reason) in cases {
            let blocked = T3CodeLaunch.blockedReason(draft, status: status, project: project, connected: connected, macName: "Studio")
            XCTAssertTrue(blocked?.contains(reason) == true, "\(String(describing: status)) \(connected): \(String(describing: blocked))")
        }
        XCTAssertEqual(T3CodeLaunch.startedNotice(macName: "Studio"), "Started in T3 Code on Studio. It appears in Chats shortly.",
                       "only when the Mac answered no conversation to open")
    }

    // Contract (I17): only a lost answer may have started the thread, so only
    // it asks before sending again; a reason from the Mac means it didn't start.
    func testOnlyALostAnswerMayHaveStarted() {
        let lost: [Error] = [
            PairingFailure.hostMessage(504, "T3 Code may have started the thread. Check T3 Code on your computer before trying again."),
            PairingFailure.response(504), PairingFailure.response(502), URLError(.timedOut), URLError(.networkConnectionLost),
        ]
        for error in lost { XCTAssertTrue(T3CodeLaunch.mayHaveStarted(error), "\(error)") }
        let refused: [Error] = [
            PairingFailure.hostMessage(409, "T3 Code no longer accepts Cupola. Connect T3 Code again in Settings."),
            PairingFailure.hostMessage(503, "T3 Code isn't running on your computer. Open it and try again."),
            PairingFailure.hostMessage(502, "T3 Code didn't start the thread. Try again."),
            PairingFailure.response(401), PairingFailure.response(404),
        ]
        for error in refused { XCTAssertFalse(T3CodeLaunch.mayHaveStarted(error), "\(error)") }
    }

    // A launch that may have started asks before sending again, survives a
    // saved-draft round trip, and stops asking once the owner confirms,
    // turns T3 Code off or the thread starts. Clearing and retyping still asks.
    func testAnUnconfirmedLaunchAsksUntilResolved() throws {
        var draft = NewChatDraft(destination: .project(id: "project"), text: "Plan the release")
        draft.startsInT3Code = true
        XCTAssertNil(T3CodeLaunch.unconfirmedLaunch(draft))
        draft.t3LaunchUnconfirmed = "Check T3 Code on Studio before trying again."
        let reloaded = try JSONDecoder().decode(NewChatDraft.self, from: JSONEncoder().encode(draft))
        XCTAssertEqual(T3CodeLaunch.unconfirmedLaunch(reloaded), "Check T3 Code on Studio before trying again.")

        var edited = reloaded; edited.text += " today"
        XCTAssertNotNil(T3CodeLaunch.unconfirmedLaunch(edited), "editing the words still asks")
        var off = reloaded; off.startsInT3Code = nil
        XCTAssertNil(T3CodeLaunch.unconfirmedLaunch(off), "a Cupola thread has a retry key")
        var confirmed = reloaded; T3CodeLaunch.confirmResend(&confirmed)
        XCTAssertNil(T3CodeLaunch.unconfirmedLaunch(confirmed))
        var cleared = reloaded; cleared.text = " "
        XCTAssertEqual(cleared.t3LaunchUnconfirmed, reloaded.t3LaunchUnconfirmed)
        cleared.text = "Retyped release plan"
        XCTAssertNotNil(T3CodeLaunch.unconfirmedLaunch(cleared), "clearing then retyping never authorizes a duplicate launch")
        var started = reloaded; started.completeSubmission()
        XCTAssertNil(started.t3LaunchUnconfirmed)
    }

    // A lost answer arrives after the owner reopened New chat and kept
    // typing: the mark joins the newer words instead of restoring the sent ones.
    func testALateLostAnswerKeepsNewerWords() {
        var launched = NewChatDraft(destination: .project(id: "project"), text: "Plan the release")
        launched.startsInT3Code = true
        let message = "Check T3 Code on Studio before trying again."
        var edited = launched; edited.text = "Plan the release for Friday"
        XCTAssertEqual(T3CodeLaunch.markingUnconfirmed(launched, message: message, current: edited, saved: edited).text,
                       "Plan the release for Friday")
        XCTAssertEqual(T3CodeLaunch.unconfirmedLaunch(T3CodeLaunch.markingUnconfirmed(launched, message: message, current: edited, saved: edited)),
                       message)
        // New chat moved to another project: that project's words stay, and
        // the launched project's saved draft is the one that asks.
        let other = NewChatDraft(destination: .project(id: "other"), text: "Something else")
        XCTAssertEqual(T3CodeLaunch.markingUnconfirmed(launched, message: message, current: other, saved: edited).text,
                       "Plan the release for Friday")
        // A different draft took its place: only the mark is merged.
        let replaced = T3CodeLaunch.markingUnconfirmed(launched, message: message, current: other, saved: nil)
        XCTAssertEqual(replaced.text, "Something else")
        XCTAssertEqual(replaced.requestID, other.requestID)
        XCTAssertEqual(replaced.t3LaunchUnconfirmed, message)
        // Nothing saved: the sent draft is kept, marked.
        XCTAssertEqual(T3CodeLaunch.markingUnconfirmed(launched, message: message, current: nil, saved: nil).text, "Plan the release")
    }
}

