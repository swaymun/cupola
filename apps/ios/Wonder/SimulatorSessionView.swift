import SwiftUI
import UIKit
import WonderComputerView
import WonderPairing

/// A simulator on one of the paired computers, opened once it is running.
struct SimulatorChoice: Identifiable, Equatable {
    let model: ConnectionModel
    let source: ComputerSource
    /// An iPhone Duo simulator: its hinge and screens can be moved.
    let hasHinge: Bool
    /// The computer and the simulator on it.
    let id: String
    /// "iOS 27.1 · Mac mini": what the picker and header say under the name.
    let detail: String

    @MainActor init(model: ConnectionModel, source: ComputerSource, hasHinge: Bool = false) {
        self.model = model
        self.source = source
        self.hasHinge = hasHinge
        id = (model.connection?.credential.hostInstallationId ?? "current") + "|" + (source.id ?? "")
        detail = [source.runtime, model.macName].compactMap { $0 }.joined(separator: " · ")
    }

    var name: String { source.name ?? "Simulator" }
    var isPad: Bool { name.contains("iPad") }
    /// The host's simulator ID, without the `simulator:` source prefix.
    var udid: String { String((source.id ?? "").dropFirst("simulator:".count)) }

    static func == (lhs: Self, rhs: Self) -> Bool { lhs.id == rhs.id }
}

/// Which kind of device this app is on, so the simulator that looks like it
/// is listed first: the iPhone Duo simulator on a Duo, an iPad on an iPad,
/// otherwise an iPhone.
enum SimulatorPhone: Equatable {
    case duo, pad, phone

    /// `onDuo`: the window has the Duo's vertical bar (`readsVerticalBar`), on either display.
    static func resolve(onDuo: Bool) -> SimulatorPhone {
        #if WONDER_DIAGNOSTICS
        let arguments = ProcessInfo.processInfo.arguments
        if arguments.contains("-diagnostics-simulator-phone-duo") { return .duo }
        if arguments.contains("-diagnostics-simulator-phone-pad") { return .pad }
        if arguments.contains("-diagnostics-simulator-phone-iphone") { return .phone }
        #endif
        if UIDevice.current.userInterfaceIdiom == .pad { return .pad }
        return onDuo ? .duo : .phone
    }

    func matches(name: String, hasHinge: Bool) -> Bool {
        switch self {
        case .duo: hasHinge
        case .pad: name.contains("iPad")
        case .phone: !hasHinge && !name.contains("iPad")
        }
    }
}

extension EnvironmentValues {
    /// Which kind of device this window is, set by the Simulators tool.
    @Entry var simulatorPhone: SimulatorPhone = .phone
}

/// The simulator last opened on each computer. Fixtures keep it in memory so a
/// scenario never inherits the previous run's choice.
@MainActor enum SimulatorLastUsed {
    private static var memory: [String: String] = [:]
    private static func key(_ computer: String) -> String { "simulator.lastUsed." + computer }

    /// The simulator fixture's stand-in host: nothing is saved between launches.
    static var isFixture: Bool {
        #if WONDER_DIAGNOSTICS
        ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-simulator-fixture")
        #else
        false
        #endif
    }

    static func sourceID(on computer: String, fixture: Bool) -> String? {
        fixture ? memory[computer] : UserDefaults.standard.string(forKey: key(computer))
    }

    static func remember(_ sourceID: String, on computer: String, fixture: Bool) {
        if fixture { memory[computer] = sourceID } else { UserDefaults.standard.set(sourceID, forKey: key(computer)) }
    }
}

/// The Simulators tool, opened by its rail pill (the pill closes it): each
/// paired computer's simulators with Start and Stop, and a running one open
/// as a computer session whose source is the simulator, so the picture,
/// session lifetime and control lease are the computer view's own
/// (`ComputerSessionModel`); only the screen, gestures and buttons differ.
struct SimulatorSessionView: View {
    /// The computer this chat is on; listed first.
    let model: ConnectionModel
    let chat: ChatSummary
    @EnvironmentObject private var library: ConnectionLibrary
    @State private var choice: SimulatorChoice?
    /// Holds the open simulator and its session while the window moves the tool.
    let toolSessions: ToolSessionStore?

    init(model: ConnectionModel, chat: ChatSummary, sessions: ToolSessionStore? = nil) {
        self.model = model
        self.chat = chat
        toolSessions = sessions
        _choice = State(initialValue: sessions?.simulatorChoice)
    }
    @State private var verticalBar = false
    @Environment(\.companionPane) private var pane
    @Environment(\.wonderTheme) private var theme

    /// On a folded Duo the tool covers the chat and its rail, so the selected
    /// Simulators button joins the system bar and closes it, as the computer
    /// view's does; the header then carries no Close of its own.
    private var closesFromBar: Bool { SimulatorBar.supported && verticalBar && pane == nil }

    var body: some View {
        Group {
            if let choice {
                SimulatorDeviceView(choice: choice, chat: choice.model === model ? chat : ComputerSessionView.hostViewChat,
                                    computer: model, library: library, closesFromBar: closesFromBar, sessions: toolSessions,
                                    chooseAnother: { leave() },
                                    stopped: { leave() }) { open($0) }
                    // Another simulator is another session with its own lease.
                    .id(choice.id)
            } else {
                VStack(spacing: 0) {
                    SimulatorHeader(title: nil, status: nil, choice: nil, computer: model, library: library,
                                    showsClose: !closesFromBar, close: nil, hinge: nil, fullScreen: nil,
                                    select: { open($0) }, stopped: { _ in })
                    ScrollView {
                        SimulatorPickerList(computer: model, library: library, selectedID: nil,
                                            select: { open($0) }, stopped: { _ in })
                            .padding(.horizontal, 16)
                            .padding(.vertical, 8)
                            .frame(maxWidth: 560)
                            .frame(maxWidth: .infinity)
                    }
                    .accessibilityIdentifier("simulator-picker-page")
                    .modifier(SimulatorBarToolbar(shows: closesFromBar, close: { dismiss() }))
                }
            }
        }
        .background(theme.computerBackground.ignoresSafeArea())
        // The Duo's bar takes the navigation bar's place beside the content.
        .toolbar(closesFromBar ? .visible : .hidden, for: .navigationBar)
        .toolbarBackgroundVisibility(.hidden, for: .navigationBar)
        .readsVerticalBar($verticalBar)
        .environment(\.simulatorPhone, SimulatorPhone.resolve(onDuo: verticalBar))
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("simulator-view")
        #if WONDER_DIAGNOSTICS
        // `-diagnostics-simulator-open-duo` picks the fixture's iPhone Duo,
        // since native Duo taps are not drivable (#387).
        .task {
            guard choice == nil, ProcessInfo.processInfo.arguments.contains("-diagnostics-simulator-open-duo") else { return }
            open(SimulatorChoice(model: model, source: ComputerSource(
                id: "simulator:00000000-0000-0000-0000-000000000004", name: "iPhone Duo", kind: "simulator", runtime: "iOS 27.1"),
                hasHinge: true))
        }
        .background {
            if choice == nil, ProcessInfo.processInfo.arguments.contains("-diagnostics-simulator-open-duo") {
                DiagnosticDriverSurface(surface: .init(priority: 20, state: { [:] }, controls: {
                    [.init(id: "simulator-reopen-duo", label: "Open iPhone Duo", value: "", tap: {
                        open(SimulatorChoice(model: model, source: ComputerSource(
                            id: "simulator:00000000-0000-0000-0000-000000000004", name: "iPhone Duo", kind: "simulator", runtime: "iOS 27.1"),
                            hasHinge: true))
                    })]
                }, actions: [:])).frame(width: 0, height: 0).accessibilityHidden(true)
            }
        }
        // The folded Duo's bar button is not drivable natively (#387): the
        // list exposes it as the production close action.
        .background {
            if closesFromBar, choice == nil {
                DiagnosticDriverSurface(surface: .init(priority: 20, state: { [:] }, controls: {
                    [.init(id: "conversation-simulators-pill", label: "Simulators", value: "Open", tap: { dismiss() })]
                }, actions: ["dismiss": { _ in dismiss() }])).frame(width: 0, height: 0).accessibilityHidden(true)
            }
        }
        #endif
    }

    private let dismiss = ToolDismiss()

    private func open(_ next: SimulatorChoice) {
        if let id = next.source.id {
            SimulatorLastUsed.remember(id, on: next.model.computerKey, fixture: SimulatorLastUsed.isFixture)
        }
        choice = next
        toolSessions?.simulatorChoice = next
    }

    private func leave() {
        choice = nil
        toolSessions?.simulatorChoice = nil
    }
}

/// The folded Duo's system bar: the selected Simulators button, which closes the tool.
private struct SimulatorBarToolbar: ViewModifier {
    let shows: Bool
    let close: () -> Void

    func body(content: Content) -> some View {
        #if canImport(SwiftUI, _version: 8.0.85)
        if #available(iOS 27.1, *) {
            content.toolbar {
                if shows {
                    ToolbarItem(id: "simulator-rail", placement: .topBarTrailing) {
                        DuoRailControls(selection: ComposerPill.simulators.id) {
                            SimulatorsPill(isPresented: true, open: close)
                                .environment(\.composerPillsInBar, true)
                                .id(ComposerPill.simulators.id)
                        }
                    }
                    .duoRail(true)
                }
            }
        } else {
            content
        }
        #else
        content
        #endif
    }
}

enum SimulatorBar {
    /// The Duo's system bar takes items from iOS 27.1.
    static var supported: Bool {
        #if canImport(SwiftUI, _version: 8.0.85)
        if #available(iOS 27.1, *) { return true }
        #endif
        return false
    }
}

/// The rail pill that opens (and, selected, closes) the Simulators tool.
struct SimulatorsPill: View {
    let isPresented: Bool
    let open: () -> Void

    var body: some View {
        Button(action: open) {
            Label("Simulators", systemImage: ComposerPill.simulators.systemImage)
                .labelStyle(.iconOnly)
                .modifier(ComposerStatusPill(isActive: isPresented))
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("conversation-simulators-pill")
        .accessibilityLabel("Simulators")
        .accessibilityValue(isPresented ? "Open" : "Closed")
        .accessibilityAddTraits(isPresented ? .isSelected : [])
        .accessibilityHint(isPresented ? "Closes the simulators" : "Start, stop and use the simulators on your computers")
    }
}

/// The picker's name (choosing another simulator), the hinge controls on a
/// Duo, Full screen where the pane can't fill the window, and Close. The rail
/// pill (or the Duo bar's) closes the tool where there is one.
private struct SimulatorHeader: View {
    let title: String?
    let status: String?
    let choice: SimulatorChoice?
    let computer: ConnectionModel
    @ObservedObject var library: ConnectionLibrary
    /// The Close button; off where the rail's selected pill closes the tool.
    let showsClose: Bool
    /// Ends the session first; nil just dismisses.
    let close: (() async -> Void)?
    /// The Duo's hinge menu, for a simulator that has one.
    let hinge: SimulatorHingeMenu?
    /// Opens the screen over the whole window, where the pane can't expand to it.
    let fullScreen: (() -> Void)?
    let select: (SimulatorChoice) -> Void
    /// A simulator stopped from the picker.
    let stopped: (SimulatorChoice) -> Void
    private let dismiss = ToolDismiss()
    @State private var choosing = false
    @State private var menuHeight: CGFloat = 220
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @Environment(\.wonderTheme) private var theme

    var body: some View {
        ComputerHeaderBarLayout(spacing: 8) {
            CompanionExpandButton()
                .buttonStyle(.bordered)
                .buttonBorderShape(.circle)
            titleButton
                .layoutValue(key: ComputerHeaderBarLayout.Title.self, value: true)
            if let hinge { hinge }
            if let fullScreen {
                Button(action: fullScreen) {
                    Label("Full screen", systemImage: "arrow.up.left.and.arrow.down.right").labelStyle(.iconOnly)
                        .frame(minWidth: 44, minHeight: 44).contentShape(Rectangle())
                }
                .accessibilityIdentifier("simulator-full-screen")
            }
            if showsClose { closeButton }
        }
        .foregroundStyle(theme.computerForeground)
        // Level with the list rows below, so Close doesn't crowd the screen edge.
        .padding(.horizontal, 14)
        .padding(.vertical, 4)
        .frame(minHeight: 52)
        .background(theme.computerBackground)
    }

    private var closeButton: some View {
        ToolCloseControl {
            Button {
                Task {
                    await close?()
                    dismiss()
                }
            } label: {
                if dynamicTypeSize.isAccessibilitySize { Image(systemName: "xmark") } else { Text("Close") }
            }
            .buttonStyle(.bordered)
            .buttonBorderShape(.capsule)
            .accessibilityLabel("Close")
            .accessibilityIdentifier("simulator-close")
        }
    }

    @ViewBuilder private var titleButton: some View {
        if let choice, let title {
            Button { choosing = true } label: {
                VStack(spacing: 1) {
                    HStack(spacing: 6) {
                        Text(title).lineLimit(1)
                        Image(systemName: "chevron.down").font(.caption.weight(.semibold))
                    }
                    if let status { Text(status).font(.caption2).foregroundStyle(.secondary).lineLimit(1) }
                }
                .dynamicTypeSize(...DynamicTypeSize.accessibility3)
                .frame(minHeight: 44).contentShape(Rectangle())
            }
            .font(.footnote.weight(.semibold)).buttonStyle(.plain)
            .accessibilityLabel("Simulator, \(title)")
            .accessibilityValue(status ?? "")
            .accessibilityHint("Choose another simulator")
            .accessibilityIdentifier("simulator-picker")
            .popover(isPresented: $choosing, arrowEdge: .top) {
                ScrollView {
                    SimulatorPickerList(computer: computer, library: library, selectedID: choice.id,
                                        select: { next in
                        choosing = false
                        if next != choice { select(next) }
                    }, stopped: { gone in
                        choosing = false
                        if gone == choice { stopped(gone) }
                    })
                    .padding(.horizontal, 14).padding(.vertical, 8)
                    .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { menuHeight = $0 }
                }
                .scrollBounceBehavior(.basedOnSize)
                .frame(idealWidth: 320, maxWidth: 360, idealHeight: min(420, menuHeight), maxHeight: 420)
                .presentationCompactAdaptation(.popover)
            }
        } else {
            Color.clear.frame(width: 0, height: 44)
        }
    }
}

/// Every simulator on each paired computer, the one that looks like this
/// device first, each with its OS; Start and Stop work from here.
private struct SimulatorPickerList: View {
    let computer: ConnectionModel
    @ObservedObject var library: ConnectionLibrary
    let selectedID: String?
    let select: (SimulatorChoice) -> Void
    let stopped: (SimulatorChoice) -> Void
    /// Per computer: how many simulators it listed, or nil before its first answer.
    @State private var counts: [String: Int] = [:]
    @State private var computers: [ConnectionModel] = []

    var body: some View {
        let named = counts.values.filter { $0 > 0 }.count > 1
        VStack(alignment: .leading, spacing: 0) {
            ForEach(computers, id: \.computerKey) { model in
                SimulatorComputerSection(model: model, isCurrent: model === computer, selectedID: selectedID,
                                         showsName: named, select: select, stopped: stopped) {
                    counts[model.computerKey] = $0
                }
            }
            let answered = counts.count >= computers.count && !computers.isEmpty
            if !counts.values.contains(where: { $0 > 0 }) {
                if !answered {
                    ProgressView("Looking for simulators").frame(maxWidth: .infinity, minHeight: 44)
                        .accessibilityIdentifier("simulator-picker-loading")
                } else {
                    Text("No simulators found.")
                        .font(.footnote).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
                        .accessibilityIdentifier("simulator-picker-empty")
                }
            }
        }
        .font(.body).foregroundStyle(.primary).buttonStyle(.plain)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("simulator-picker-list")
        .onAppear {
            guard computers.isEmpty else { return }
            let others = library.saved.connections.filter {
                !$0.requiresPairing && $0.credential.hostInstallationId != computer.connection?.credential.hostInstallationId
            }.map { library.model(for: $0) }
            computers = [computer] + others
        }
    }
}

private extension ConnectionModel {
    var computerKey: String { connection?.credential.hostInstallationId ?? "current" }
}

/// One computer's simulators, checked every few seconds while shown. A
/// running one opens or stops; a stopped one starts, then opens once it has
/// booted. A computer that can't be reached is simply absent.
private struct SimulatorComputerSection: View {
    @ObservedObject var model: ConnectionModel
    /// The computer the chat is on (the fixture's computer).
    let isCurrent: Bool
    let selectedID: String?
    /// More than one computer has simulators, so each says whose they are.
    let showsName: Bool
    let select: (SimulatorChoice) -> Void
    let stopped: (SimulatorChoice) -> Void
    let report: (Int) -> Void
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.simulatorPhone) private var phone
    @State private var simulators: [Listed] = []
    /// The simulator being started or stopped (one at a time), since when,
    /// and why others didn't.
    @State private var working: Work?
    @State private var failures: [String: String] = [:]
    @State private var request: Request?

    /// A simulator as listed: `state` is the host's (booted, shutdown, booting, shuttingDown).
    struct Listed: Equatable, Sendable {
        let source: ComputerSource
        let state: String
        let hasHinge: Bool
        /// The host refuses to stop Cupola's own test simulators.
        var canStop = true
        var running: Bool { state == "booted" }
        var id: String { source.id ?? "" }
        var udid: String { String(id.dropFirst("simulator:".count)) }
    }
    private struct Work: Equatable {
        enum Kind { case start, stop }
        let id: String
        let kind: Kind
        let since: Date
    }
    /// Each tap is a new request, so Retry asks again.
    private struct Request: Equatable {
        let simulator: Listed
        let kind: Work.Kind
        let tap: Int
    }

    private var identity: String? {
        guard scenePhase == .active, !model.accessEnded else { return nil }
        return model.connection.map { $0.credential.hostInstallationId + ":" + $0.credential.deviceId } ?? "fixture"
    }

    private var isFixture: Bool { SimulatorLastUsed.isFixture }

    /// The simulator that looks like this device first, then the one last
    /// used here, running ones, and by name.
    private var ordered: [Listed] {
        let last = SimulatorLastUsed.sourceID(on: model.computerKey, fixture: isFixture)
        func rank(_ item: Listed) -> [Int] {
            [phone.matches(name: item.source.name ?? "", hasHinge: item.hasHinge) ? 0 : 1,
             item.id == last ? 0 : 1, item.running ? 0 : 1]
        }
        return simulators.sorted { lhs, rhs in
            let (a, b) = (rank(lhs), rank(rhs))
            return a == b ? (lhs.source.name ?? "") < (rhs.source.name ?? "") : a.lexicographicallyPrecedes(b)
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if showsName, !simulators.isEmpty {
                Text(model.macName).font(.footnote.weight(.semibold)).foregroundStyle(.secondary)
                    .padding(.top, 12).padding(.bottom, 2)
                    .accessibilityAddTraits(.isHeader)
            }
            ForEach(ordered, id: \.id) { simulator in
                row(simulator)
            }
        }
        .task(id: identity) {
            #if WONDER_DIAGNOSTICS
            if let fixture = Self.fixture {
                simulators = isCurrent ? fixture : []
                report(simulators.count)
                return
            }
            #endif
            guard let identity, model.connection != nil else { report(0); return }
            while !Task.isCancelled {
                do {
                    let listed = try await Self.listed(on: model)
                    guard !Task.isCancelled, identity == self.identity else { return }
                    simulators = listed
                    report(simulators.count)
                } catch {
                    guard !Task.isCancelled, identity == self.identity else { return }
                    simulators = []
                    report(0)
                }
                do { try await Task.sleep(for: .seconds(5)) } catch { return }
            }
        }
        // Leaving the picker ends the wait; the computer finishes on its own.
        .task(id: request) {
            guard let request else { return }
            let simulator = request.simulator
            working = Work(id: simulator.id, kind: request.kind, since: Date())
            failures[simulator.id] = nil
            do {
                switch request.kind {
                case .start: try await start(simulator)
                case .stop: try await stop(simulator)
                }
                guard !Task.isCancelled else { return }
                working = nil
                let state = request.kind == .start ? "booted" : "shutdown"
                simulators = simulators.map {
                    $0.id == simulator.id ? Listed(source: $0.source, state: state, hasHinge: $0.hasHinge, canStop: $0.canStop) : $0
                }
                let choice = SimulatorChoice(model: model, source: simulator.source, hasHinge: simulator.hasHinge)
                if request.kind == .start { select(choice) } else { stopped(choice) }
            } catch {
                guard !Task.isCancelled else { return }
                working = nil
                failures[simulator.id] = Self.message(error, request.kind)
            }
        }
        .onDisappear { working = nil }
    }

    /// The host's simulator list (with each one's OS), or on a host from
    /// before it, the computer view's booted-simulator sources.
    private static func listed(on model: ConnectionModel) async throws -> [Listed] {
        do {
            let list: SimulatorList = try await model.manage("/api/v1/simulators")
            return list.simulators.filter { $0.state == "booted" || !$0.inUseByTests }.map {
                Listed(source: ComputerSource(id: $0.sourceId, name: $0.name, kind: "simulator", runtime: $0.runtime),
                       state: $0.state, hasHinge: ($0.hasHinge ?? false) && ($0.canStop ?? true), canStop: $0.canStop ?? true)
            }
        } catch let error where Self.routeMissing(error) {
            let result: ComputerAvailableSources = try await model.manage("/api/v1/computer/sources/simulators")
            return result.sources.filter { $0.kind == "simulator" && $0.id != nil }.map {
                Listed(source: $0, state: "booted", hasHinge: ($0.name ?? "").contains("Duo"))
            }
        }
    }

    /// An older host has no simulator list (404, or 405 behind a catch-all).
    private static func routeMissing(_ error: Error) -> Bool {
        switch error {
        case PairingFailure.response(let status), PairingFailure.hostMessage(let status, _): [404, 405].contains(status)
        default: false
        }
    }

    /// Asks the computer to boot it and waits until it has.
    private func start(_ simulator: Listed) async throws {
        #if WONDER_DIAGNOSTICS
        if Self.fixture != nil {
            try await Task.sleep(for: .seconds(8))
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-simulator-boot-fails") {
                throw PairingFailure.hostMessage(502, "The simulator couldn’t be started. Try again.")
            }
            return
        }
        #endif
        let _: SimulatorState = try await model.manage("/api/v1/simulators/\(simulator.udid)/boot", method: "POST", body: Data("{}".utf8))
    }

    private func stop(_ simulator: Listed) async throws {
        #if WONDER_DIAGNOSTICS
        if Self.fixture != nil {
            try await Task.sleep(for: .seconds(5))
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-simulator-stop-fails") {
                throw PairingFailure.hostMessage(502, "The simulator couldn’t be stopped. Try again.")
            }
            return
        }
        #endif
        let _: SimulatorState = try await model.manage("/api/v1/simulators/\(simulator.udid)/shutdown", method: "POST", body: Data("{}".utf8))
    }

    /// The host's words when it gave any; a host from before starting has no such route.
    private static func message(_ error: Error, _ kind: Work.Kind) -> String {
        switch error {
        case PairingFailure.hostMessage(_, let message): message
        case PairingFailure.response(let status) where [404, 405].contains(status):
            "Update Cupola on your computer to start and stop simulators from here."
        case is URLError: "Your computer can’t be reached right now."
        default: kind == .start ? "The simulator couldn’t be started. Try again." : "The simulator couldn’t be stopped. Try again."
        }
    }

    /// `GET /api/v1/simulators` (wonder-http-v1 `simulatorList`), the fields shown here.
    private struct SimulatorList: Decodable, Sendable {
        struct Simulator: Decodable, Sendable {
            let sourceId: String
            let name: String
            let runtime: String
            let state: String
            let inUseByTests: Bool
            /// Absent from hosts before hinge control.
            let hasHinge: Bool?
            let canStop: Bool?
        }
        let simulators: [Simulator]
    }

    /// `POST /api/v1/simulators/{udid}/boot` (`simulatorStateResult`): the state once it answers.
    private struct SimulatorState: Decodable, Sendable { let state: String }

    private func row(_ simulator: Listed) -> some View {
        let choice = SimulatorChoice(model: model, source: simulator.source, hasHinge: simulator.hasHinge)
        let checked = choice.id == selectedID
        let mine = working?.id == simulator.id ? working : nil
        let failure = mine == nil ? failures[simulator.id] : nil
        // Started or stopped elsewhere: the state says so, though nothing here is waiting.
        let state: String? = mine.map { $0.kind == .start ? "Starting" : "Stopping" }
            ?? (simulator.state == "booting" ? "Starting" : simulator.state == "shuttingDown" ? "Stopping" : nil)
        let stoppable = simulator.running && simulator.canStop && state == nil
        return HStack(spacing: 0) {
            Button {
                if simulator.running { select(choice) }
                else if state == nil { request = Request(simulator: simulator, kind: .start, tap: (request?.tap ?? 0) + 1) }
            } label: {
                HStack(spacing: 10) {
                    Image(systemName: choice.isPad ? "ipad" : "iphone").frame(width: 24).accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 2) {
                        HStack(spacing: 6) {
                            Text(choice.name).lineLimit(2)
                            // Running is a green dot after the name; the trailing Stop says the same in words.
                            if simulator.running {
                                Circle().fill(Color.green).frame(width: 8, height: 8).accessibilityHidden(true)
                            }
                        }
                        Text(failure ?? [simulator.source.runtime, simulator.running ? "Booted" : state == nil ? "Off" : nil].compactMap { $0 }.joined(separator: " · ")).font(.caption).foregroundStyle(.secondary)
                            .lineLimit(failure == nil ? 1 : 3)
                            .fixedSize(horizontal: false, vertical: failure != nil)
                    }
                    Spacer(minLength: 8)
                    if let state {
                        SimulatorProgress(label: state, since: mine?.since)
                    } else if checked { Image(systemName: "checkmark").accessibilityHidden(true) }
                    else if !simulator.running {
                        Text(failure == nil ? "Boot" : "Retry").font(.footnote.weight(.semibold)).foregroundStyle(.tint)
                    }
                }
                .padding(.vertical, 4).frame(minHeight: 52).contentShape(Rectangle())
            }
            // One simulator changes at a time; running ones stay openable meanwhile.
            .disabled(working != nil && !simulator.running)
            .accessibilityLabel(choice.name + ", " + choice.detail + (simulator.running ? "" : ", stopped"))
            .accessibilityValue(state ?? failure ?? (checked ? "Selected" : ""))
            .accessibilityHint(simulator.running ? "" : failure == nil ? "Starts it, then opens it" : "Tries starting it again")
            .accessibilityIdentifier("simulator-choice:" + (simulator.source.id ?? ""))
            if stoppable {
                Button {
                    request = Request(simulator: simulator, kind: .stop, tap: (request?.tap ?? 0) + 1)
                } label: {
                    Text("Stop").font(.footnote.weight(.semibold)).foregroundStyle(.tint)
                        .frame(minWidth: 44, minHeight: 44, alignment: .trailing).contentShape(Rectangle())
                }
                .disabled(working != nil)
                .accessibilityLabel("Stop " + choice.name)
                .accessibilityHint("Shuts the simulator down")
                .accessibilityIdentifier("simulator-stop:" + (simulator.source.id ?? ""))
            }
        }
    }

    #if WONDER_DIAGNOSTICS
    /// `-diagnostics-computer-simulator-fixture`: two running simulators and a
    /// running iPhone Duo on the fixture computer, plus a stopped one with
    /// `-diagnostics-simulator-startable` (Start waits, then opens it, or with
    /// `-diagnostics-simulator-boot-fails` fails; Stop waits, or with
    /// `-diagnostics-simulator-stop-fails` fails); with `-diagnostics-simulator-none`, none;
    /// `-diagnostics-simulator-duo-in-use` makes the Duo a pooled test lane.
    private static var fixture: [Listed]? {
        let arguments = ProcessInfo.processInfo.arguments
        guard arguments.contains("-diagnostics-computer-simulator-fixture") else { return nil }
        if arguments.contains("-diagnostics-simulator-none") { return [] }
        // A pooled test lane: no Stop and no hinge menu, as the host reports.
        let pooled = arguments.contains("-diagnostics-simulator-duo-in-use")
        var listed = [
            Listed(source: ComputerSource(id: "simulator:00000000-0000-0000-0000-000000000001", name: "iPhone 17 Pro", kind: "simulator", runtime: "iOS 26.5"), state: "booted", hasHinge: false),
            Listed(source: ComputerSource(id: "simulator:00000000-0000-0000-0000-000000000002", name: "iPad Air 13-inch (M3)", kind: "simulator", runtime: "iOS 27.1"), state: "booted", hasHinge: false),
            Listed(source: ComputerSource(id: "simulator:00000000-0000-0000-0000-000000000004", name: "iPhone Duo", kind: "simulator", runtime: "iOS 27.1"), state: "booted",
                   hasHinge: !pooled, canStop: !pooled),
        ]
        if arguments.contains("-diagnostics-simulator-startable") {
            listed.append(Listed(source: ComputerSource(id: "simulator:00000000-0000-0000-0000-000000000003", name: "iPhone 17", kind: "simulator", runtime: "iOS 27.1"), state: "shutdown", hasHinge: false))
        }
        return listed
    }
    #endif
}

/// "Starting…" or "Stopping…" with a spinner. A boot has no real percentage,
/// so after a few seconds it says how long it has been going instead.
private struct SimulatorProgress: View {
    let label: String
    let since: Date?

    var body: some View {
        HStack(spacing: 6) {
            ProgressView().controlSize(.small)
            if let since {
                TimelineView(.periodic(from: since, by: 1)) { context in
                    let seconds = Int(context.date.timeIntervalSince(since))
                    Text(seconds >= 8 ? "\(label)… \(seconds) s" : "\(label)…").monospacedDigit()
                }
            } else {
                Text(label + "…")
            }
        }
        .font(.footnote).foregroundStyle(.secondary)
    }
}

/// One simulator's screen, its buttons and Take control.
private struct SimulatorDeviceView: View {
    let choice: SimulatorChoice
    let computer: ConnectionModel
    @ObservedObject var library: ConnectionLibrary
    /// The folded Duo's bar carries the selected Simulators button, so there is no Close here.
    let closesFromBar: Bool
    /// The list of simulators, after this one failed.
    let chooseAnother: () -> Void
    /// This simulator was stopped from the picker.
    let stopped: () -> Void
    let select: (SimulatorChoice) -> Void
    @StateObject private var session: ComputerSessionModel
    @StateObject private var posture: SimulatorPosture
    /// Quarter turns of the picture, remembered for this simulator.
    @State private var quarterTurns: Int
    /// The screen over the whole window, where the pane can't expand to it.
    @State private var fullScreen = false
    /// Set by `watchPicture`: the first frame has arrived / none came in time.
    @State private var hasFrame = false
    @State private var pictureLate = false
    private let dismiss = ToolDismiss()
    @Environment(\.companionPane) private var pane
    @Environment(\.verticalSizeClass) private var verticalSizeClass
    @Environment(\.wonderTheme) private var theme

    /// What the pane shows: one value derived from the session, the receiver
    /// and the picture clock, so the spinner, the picture and every notice
    /// follow it and none can be drawn together or be missing (SM-03: a live
    /// connection with no picture left a black pane, and a notice that could
    /// not be retried said "Opening…" forever).
    private enum PanePhase {
        /// Connecting, or the computer is still preparing the view.
        case opening
        /// Connected, no frame yet.
        case waitingForPicture
        /// No picture within the budget: Try again.
        case noPicture
        /// The view failed or was interrupted: the host's words and Try again.
        case failed(ComputerViewNotice)
        /// The computer can't be reached; the view reconnects by itself.
        case offline
        /// A fact with no action: paused, or viewing unavailable on this computer.
        case message(ComputerViewNotice)
        case closed
        /// A frame is arriving for a screen of known size.
        case showing

        var isShowing: Bool {
            if case .showing = self { return true }
            return false
        }
    }
    /// How long a live connection may go without a picture before it says so.
    private static let pictureGrace: Duration = .seconds(10)
    /// How long the view may take to go live. The computer ends a view that
    /// opens slower than 20 s with its own reason; this covers a computer
    /// that does not.
    private static let openingBudget: Duration = .seconds(25)

    private var phase: PanePhase {
        if let notice = session.notice {
            if notice.canRetry { return choice.model.macConnected == false ? .offline : .failed(notice) }
            if session.isClosed { return .closed }
            if session.isPaused || session.session?.state == .unavailable || session.session?.capability.available == false {
                return .message(notice)
            }
            return pictureLate ? .noPicture : .opening
        }
        if session.isClosed { return .closed }
        guard session.receiverState == .live else { return pictureLate ? .noPicture : .opening }
        guard hasFrame, SimulatorScreen.nativeSize(of: session) != nil else {
            return pictureLate ? .noPicture : .waitingForPicture
        }
        return .showing
    }

    init(choice: SimulatorChoice, chat: ChatSummary, computer: ConnectionModel, library: ConnectionLibrary, closesFromBar: Bool,
         sessions: ToolSessionStore? = nil, chooseAnother: @escaping () -> Void, stopped: @escaping () -> Void, select: @escaping (SimulatorChoice) -> Void) {
        self.choice = choice
        self.computer = computer
        self.library = library
        self.closesFromBar = closesFromBar
        self.chooseAnother = chooseAnother
        self.stopped = stopped
        self.select = select
        let held = sessions?.simulator(choice.id) {
            (Self.newSession(choice: choice, chat: chat), SimulatorPosture(choice: choice))
        } ?? (Self.newSession(choice: choice, chat: chat), SimulatorPosture(choice: choice))
        _posture = StateObject(wrappedValue: held.1)
        _session = StateObject(wrappedValue: held.0)
        #if WONDER_DIAGNOSTICS
        // Fixture simulators forget their rotation when the app launches (not when the view
        // reopens), so a scenario starts upright and still proves reopening keeps it.
        if (choice.source.id ?? "").hasPrefix("simulator:00000000-0000-0000-0000-"), DiagnosticSessionBuilds.fixtureRotationReset.insert(choice.id).inserted {
            UserDefaults.standard.removeObject(forKey: Self.rotationKey(choice))
        }
        #endif
        _quarterTurns = State(initialValue: UserDefaults.standard.integer(forKey: Self.rotationKey(choice)))
    }

    private static func newSession(choice: SimulatorChoice, chat: ChatSummary) -> ComputerSessionModel {
        #if WONDER_DIAGNOSTICS
        DiagnosticSessionBuilds.simulator += 1
        #endif
        return ComputerSessionModel(model: choice.model, chat: chat, viewTarget: .simulator(choice.source))
    }

    private static func rotationKey(_ choice: SimulatorChoice) -> String { "simulator.quarterTurns." + (choice.source.id ?? "") }

    var body: some View {
        VStack(spacing: 0) {
            SimulatorHeader(title: choice.name, status: status, choice: choice, computer: computer, library: library,
                            showsClose: !closesFromBar, close: { await session.close() },
                            hinge: choice.hasHinge ? SimulatorHingeMenu(posture: posture) : nil,
                            fullScreen: offersFullScreen ? { fullScreen = true } : nil,
                            select: select, stopped: { _ in stopped() })
            SimulatorScreen(session: session, posture: posture, name: choice.name, quarterTurns: quarterTurns,
                            showsPicture: phase.isShowing)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .overlay { paneContent }
            if let message = posture.message {
                Text(message)
                    .font(.caption)
                    .foregroundStyle(theme.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 4)
                    .accessibilityIdentifier("simulator-hinge-message")
            }
            SimulatorControls(session: session, rotate: rotate, hasPicture: phase.isShowing)
        }
        .background(theme.computerBackground)
        .modifier(SimulatorBarToolbar(shows: closesFromBar) {
            Task { await session.close(); dismiss() }
        })
        .fullScreenCover(isPresented: $fullScreen) { fullScreenView }
        // Outside the controls so their changes keep its focus and composition.
        .background(alignment: .bottomLeading) {
            ComputerNativeKeyboard(
                presented: session.isControlActive && session.keyboardPresented,
                resetID: session.keyboardRevision,
                hostReference: choice.name,
                onActions: { session.sendKeyboardActions($0) },
                onDismiss: { session.keyboardPresented = false }
            )
            .frame(width: 1, height: 1)
            .clipped()
        }
        #if WONDER_DIAGNOSTICS
        .onAppear { DiagnosticSessionBuilds.simulatorAppearances += 1 }
        #endif
        .modifier(ComputerSessionLifetime(model: choice.model, session: session, chatID: session.chat.id) {
            await session.close()
            dismiss()
        })
        .onChange(of: session.isControlActive) { _, active in
            if active, ComputerSessionModel.hardwareKeyboardTyping { session.keyboardPresented = true }
        }
        // The host answers whether input works once its helper has checked
        // this simulator; look again while the view is only viewable.
        .task(id: session.isViewOnly && session.receiverState == .live) {
            guard session.isViewOnly, session.receiverState == .live, !session.isFixture else { return }
            while !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(5)) } catch { return }
                await session.refresh()
                if !session.isViewOnly { return }
            }
        }
        .task(id: PictureClock(sessionID: session.session?.id, live: session.receiverState == .live)) { await watchPicture() }
        #if WONDER_DIAGNOSTICS
        .background {
            // Frames still arriving (not a reconnect or opening state), the saved
            // rotation, and how many session models this process has built.
            DiagnosticDriverSurface(surface: .init(priority: 20, state: {
                ["simulator": ["showing": phase.isShowing, "hasPicture": hasFrame, "quarterTurns": quarterTurns,
                               "sessionBuilds": DiagnosticSessionBuilds.simulator, "appearances": DiagnosticSessionBuilds.simulatorAppearances,
                               "controlling": session.isControlActive]]
            }, controls: { [.init(id: "simulator-rotate", label: "Rotate view", value: "", tap: { rotate() }),
                             .init(id: "simulator-choose-another", label: "Choose another simulator", value: "", tap: { chooseAnother() }),
                             // This surface owns the driver's controls while it is open, so it
                             // keeps the rail pill's close that scenarios already drive.
                             .init(id: "conversation-simulators-pill", label: "Simulators", value: "Open", tap: { dismiss() })] },
               actions: [:])).frame(width: 0, height: 0).accessibilityHidden(true)
        }
        // `signal simulator-cover` and `simulator-uncover` stand in for Full
        // screen and its exit: native Duo taps are not drivable (#387).
        .task {
            guard session.isFixture else { return }
            var covered = false, uncovered = false
            while !Task.isCancelled {
                if !covered, DiagnosticSubagentFixture.signalled("simulator-cover") { covered = true; fullScreen = true }
                if !uncovered, DiagnosticSubagentFixture.signalled("simulator-uncover") { uncovered = true; fullScreen = false }
                try? await Task.sleep(for: .milliseconds(100))
            }
        }
        // `signal simulator-type-unsupported`: the keyboard sends text a
        // simulator can't type (the simulator's AXe input is US-ASCII only).
        .task {
            guard session.isFixture else { return }
            while !DiagnosticSubagentFixture.signalled("simulator-type-unsupported") {
                do { try await Task.sleep(for: .milliseconds(100)) } catch { return }
            }
            session.sendKeyboardActions([.text("café ☕")])
        }
        #endif
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("simulator-device")
    }

    private var status: String {
        let live: String
        switch phase {
        case .failed, .offline: live = "Unavailable"
        case .noPicture: live = "No picture"
        case .showing: live = session.liveStatus
        case .opening, .waitingForPicture, .message, .closed: live = "Opening"
        }
        return [choice.detail, live].filter { !$0.isEmpty }.joined(separator: " · ")
    }

    private func rotate() {
        quarterTurns = (quarterTurns + 1) % 4
        UserDefaults.standard.set(quarterTurns, forKey: Self.rotationKey(choice))
    }

    /// Opening, or why it failed and what to do: the host's words, then ours.
    @ViewBuilder private var paneContent: some View {
        switch phase {
        case .showing:
            EmptyView()
        case .opening:
            spinner("Opening \(choice.name)…", id: "simulator-loading")
        case .waitingForPicture:
            spinner("Waiting for the picture…", id: "simulator-waiting-picture")
        case .offline:
            // Unreachable computer: its name and a red dot. The view reconnects by itself.
            VStack(spacing: 10) {
                Image(systemName: choice.isPad ? "ipad" : "iphone").font(.system(size: 34, weight: .light)).accessibilityHidden(true)
                HStack(spacing: 6) {
                    Text(choice.model.macName).font(.headline)
                    Circle().fill(Color.red).frame(width: 8, height: 8).accessibilityHidden(true)
                }
            }
            .foregroundStyle(theme.computerForeground)
            .accessibilityElement(children: .combine)
            .accessibilityLabel("\(choice.model.macName), offline")
            .accessibilityIdentifier("simulator-offline")
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(theme.computerBackground)
        case let .failed(notice):
            ComputerViewNoticeView(
                notice: notice, busy: session.isLoading, symbol: choice.isPad ? "ipad" : "iphone",
                recovery: "Make sure it’s still running on \(session.hostHere), then try again or choose another simulator.",
                identifierPrefix: "simulator", retry: { Task { await session.retry() } },
                alternative: ("Choose another simulator", chooseAnother))
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(theme.computerBackground)
        case let .message(notice):
            ComputerViewNoticeView(notice: notice, busy: false, symbol: choice.isPad ? "ipad" : "iphone",
                                   identifierPrefix: "simulator", retry: {})
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(theme.computerBackground)
        case .noPicture:
            ComputerViewNoticeView(
                notice: ComputerViewNotice(message: "No picture from \(choice.name) yet", canRetry: true),
                busy: session.isLoading, symbol: choice.isPad ? "ipad" : "iphone",
                recovery: "Make sure its screen is on, then try again or choose another simulator.",
                identifierPrefix: "simulator-picture", retry: { Task { await session.retry() } },
                alternative: ("Choose another simulator", chooseAnother))
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(theme.computerBackground)
        case .closed:
            EmptyView()
        }
    }

    private func spinner(_ title: String, id: String) -> some View {
        VStack(spacing: 12) {
            ProgressView().controlSize(.large)
            Text(title).font(.headline)
        }
        .foregroundStyle(theme.computerForeground)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(theme.computerBackground)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier(id)
    }

    private var offersFullScreen: Bool {
        pane?.onDisplayWithFold == true || (pane == nil && verticalSizeClass == .compact)
    }

    /// The picture, with Exit full screen, over the whole window; the
    /// buttons stay under it.
    private var fullScreenView: some View {
        VStack(spacing: 0) {
            SimulatorScreen(session: session, posture: posture, name: choice.name, quarterTurns: quarterTurns,
                            showsPicture: phase.isShowing)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .overlay { paneContent }
                .overlay(alignment: .topLeading) {
                    Button { fullScreen = false } label: {
                        Image(systemName: "arrow.down.right.and.arrow.up.left")
                            .frame(width: 44, height: 44).contentShape(Rectangle())
                    }
                    .background(.regularMaterial, in: Circle())
                    .padding(8)
                    .accessibilityLabel("Exit full screen")
                    .accessibilityIdentifier("simulator-exit-full-screen")
                }
            SimulatorControls(session: session, rotate: rotate, hasPicture: phase.isShowing)
        }
        .foregroundStyle(theme.computerForeground)
        .background(theme.computerBackground.ignoresSafeArea())
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("simulator-full-screen-view")
    }

    private func hasPicture() -> Bool {
        #if WONDER_DIAGNOSTICS
        // `-diagnostics-simulator-no-picture`: the fixture connects live and
        // sends no frame until `signal simulator-picture`.
        if session.isFixture {
            return !ProcessInfo.processInfo.arguments.contains("-diagnostics-simulator-no-picture")
                || DiagnosticSubagentFixture.signalled("simulator-picture")
        }
        #endif
        return session.receiver.picture.hasFrame
    }

    /// What restarts the picture clock: a new view, or this one going live.
    private struct PictureClock: Equatable { let sessionID: String?; let live: Bool }

    /// Waits for the first frame. A connection that goes live gets
    /// `pictureGrace` to show one; one that is not live yet gets
    /// `openingBudget`. Either ends in a notice with Try again, not a black
    /// pane, and a frame that arrives late still replaces it.
    private func watchPicture() async {
        let live = session.receiverState == .live
        let budget = live ? Self.pictureGrace : Self.openingBudget
        let start = ContinuousClock.now
        hasFrame = false
        pictureLate = false
        while !Task.isCancelled {
            if live, hasPicture() { hasFrame = true; pictureLate = false; return }
            pictureLate = ContinuousClock.now - start >= budget
            do { try await Task.sleep(for: .milliseconds(250)) } catch { return }
        }
    }
}

/// Take control, or the toolbar while controlling: Home, Lock, volume, the
/// keyboard, the picture's rotation and Give control back.
private struct SimulatorControls: View {
    @ObservedObject var session: ComputerSessionModel
    let rotate: () -> Void
    /// Take control waits for the first frame: over a black pane it offers control of nothing.
    let hasPicture: Bool
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize

    var body: some View {
        VStack(spacing: 0) {
            if let message = session.controlMessage, session.notice?.canRetry != true {
                Text(message)
                    .font(.caption)
                    .foregroundStyle(theme.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 12)
                    .padding(.top, 6)
                    .accessibilityIdentifier("simulator-control-message")
            }
            if session.isControlActive {
                HStack(spacing: 2) {
                    tool("Home", symbol: "house", id: "home") { session.pressSimulatorButton("home") }
                    tool("Lock", symbol: "lock", id: "lock") { session.pressSimulatorButton("lock") }
                    tool("Volume down", symbol: "speaker.minus", id: "volume-down") { session.pressSimulatorButton("volumeDown") }
                    tool("Volume up", symbol: "speaker.plus", id: "volume-up") { session.pressSimulatorButton("volumeUp") }
                    tool(session.keyboardPresented ? "Hide keyboard" : "Show keyboard",
                         symbol: session.keyboardPresented ? "keyboard.chevron.compact.down" : "keyboard", id: "keyboard") {
                        session.toggleKeyboard()
                    }
                    rotateButton
                    Button { Task { await session.done() } } label: {
                        Group {
                            if dynamicTypeSize.isAccessibilitySize { Image(systemName: "arrow.uturn.backward") }
                            else {
                                ViewThatFits(in: .horizontal) {
                                    Text("Give control back").lineLimit(1)
                                    Image(systemName: "arrow.uturn.backward")
                                }
                            }
                        }
                        .frame(minWidth: 44, minHeight: 44)
                        .padding(.horizontal, 6)
                        .contentShape(Rectangle())
                    }
                    .layoutPriority(1)
                    .accessibilityLabel("Give control back")
                    .accessibilityIdentifier("simulator-give-back")
                }
                .accessibilityElement(children: .contain)
                .accessibilityLabel("Simulator control active")
                .accessibilityIdentifier("simulator-controls-active")
            } else if session.isStarting {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Starting control…").font(.footnote)
                }
                .frame(maxWidth: .infinity, minHeight: 44)
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("simulator-control-starting")
            } else if session.isViewOnly {
                HStack(spacing: 8) {
                    Text("View only. " + session.viewOnlyReason)
                        .font(.footnote)
                        .foregroundStyle(theme.computerForeground)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
                        .accessibilityIdentifier("simulator-view-only")
                    rotateButton.frame(width: 48)
                }
                .padding(.horizontal, 6)
            } else if session.controlAvailable && hasPicture {
                // The same centred Take control as the computer view's.
                HStack {
                    Color.clear.frame(width: 48, height: 1)
                    Spacer(minLength: 0)
                    ComputerControlChip(model: session, agent: nil, takeControlID: "simulator-take-control")
                    Spacer(minLength: 0)
                    rotateButton.frame(width: 48)
                }
            } else {
                // Nothing to offer yet. The bar keeps its height so the
                // picture does not resize when the first frame brings Take control.
                Color.clear.frame(height: 44)
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .font(.body)
        .foregroundStyle(theme.accent)
        .buttonStyle(.plain)
        // No container identifier here: it would replace the active row's.
        .background(session.isControlActive ? theme.surface : theme.computerBackground)
    }

    private var rotateButton: some View {
        tool("Rotate view", symbol: "rotate.right", id: "rotate", action: rotate)
            .accessibilityHint("Turns the picture a quarter turn; the simulator keeps its orientation")
    }

    private func tool(_ label: String, symbol: String, id: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol)
                .frame(minWidth: 44, maxWidth: .infinity, minHeight: 44)
                .contentShape(Rectangle())
        }
        .accessibilityLabel(label)
        .accessibilityIdentifier("simulator-" + id)
    }
}

/// The simulator's screen at its native aspect, fitted and optionally turned,
/// with one-finger touch while controlling.
private struct SimulatorScreen: View {
    @ObservedObject var session: ComputerSessionModel
    /// Only the fixture's stand-in picture shows the posture; a real one streams it.
    @ObservedObject var posture: SimulatorPosture
    let name: String
    let quarterTurns: Int
    /// The pane's phase says a frame is arriving; otherwise nothing is drawn,
    /// not even the black plate the picture sits on.
    let showsPicture: Bool
    @Environment(\.wonderTheme) private var theme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    /// The framebuffer's size: the host's content rectangle, else the source size.
    static func nativeSize(of session: ComputerSessionModel) -> CGSize? {
        guard let source = session.session?.source, source.kind == "simulator" else { return nil }
        if let crop = source.crop, crop.width > 0, crop.height > 0 { return CGSize(width: crop.width, height: crop.height) }
        guard let width = source.width, let height = source.height, width > 0, height > 0 else { return nil }
        return CGSize(width: Double(width), height: Double(height))
    }

    var body: some View {
        GeometryReader { proxy in
            if showsPicture, let nativeSize = Self.nativeSize(of: session) {
                let geometry = SimulatorScreenGeometry(viewport: proxy.size, nativeSize: nativeSize,
                                                       quarterTurns: quarterTurns, inset: 12)
                let corner = min(max(min(geometry.screen.width, geometry.screen.height) * 0.06, 6), 28)
                ZStack {
                    picture(size: geometry.unrotatedSize, corner: corner)
                        .rotationEffect(.degrees(Double(geometry.quarterTurns) * 90))
                        .position(x: geometry.screen.midX, y: geometry.screen.midY)
                    RoundedRectangle(cornerRadius: corner, style: .continuous)
                        .strokeBorder(theme.rule, lineWidth: 1)
                        .frame(width: geometry.screen.width, height: geometry.screen.height)
                        .position(x: geometry.screen.midX, y: geometry.screen.midY)
                        .allowsHitTesting(false)
                        .accessibilityHidden(true)
                    SimulatorTouchSurface(
                        geometry: geometry,
                        name: name,
                        state: accessibilityState,
                        controlling: session.isControlActive,
                        canTakeControl: session.canTakeControl,
                        touch: { session.simulatorTouch($0) },
                        gesture: { session.simulatorGesture($0) },
                        home: { session.pressSimulatorButton("home") },
                        takeControl: { session.takeControl() }
                    )
                    .frame(width: geometry.screen.width, height: geometry.screen.height)
                    .position(x: geometry.screen.midX, y: geometry.screen.midY)
                }
                .animation(reduceMotion ? nil : .snappy(duration: 0.25), value: quarterTurns)
            }
        }
        #if WONDER_DIAGNOSTICS
        .overlay(alignment: .topLeading) {
            Color.clear.frame(width: 1, height: 1)
                .accessibilityElement()
                .accessibilityIdentifier("simulator-fixture-input")
                .accessibilityValue(Self.describe(session.fixtureSentInput))
        }
        #endif
    }

    private var accessibilityState: String {
        if session.isControlActive { return "You’re controlling it" }
        if session.isViewOnly { return "View only" }
        return session.receiverState == .live ? "Live" : session.receiverState.title
    }

    private func picture(size: CGSize, corner: CGFloat) -> some View {
        let video = ComputerViewportTransform(size: size, aspect: size.width / max(size.height, 1), zoom: 1)
            .videoFrame(encodedAspect: ComputerSessionModel.encodedAspectRatio(session.session))
        return ZStack {
            Color.black
            #if WONDER_DIAGNOSTICS
            if session.isFixture { SimulatorFixtureBackdrop(hinge: posture.hinge, flipped: posture.flipped == true) }
            #endif
            if session.session?.capability.available == true && !session.isFixture {
                ComputerVideoView(receiver: session.receiver)
                    .frame(width: video.width, height: video.height)
                    .position(x: video.midX, y: video.midY)
            }
        }
        .frame(width: size.width, height: size.height)
        .clipShape(RoundedRectangle(cornerRadius: corner, style: .continuous))
        .allowsHitTesting(false)
        .accessibilityHidden(true)
    }

    #if WONDER_DIAGNOSTICS
    /// What the fixture's stand-in host received, newest last, for scenarios.
    static func describe(_ actions: [ComputerInputAction]) -> String {
        var words: [String] = []
        for action in actions {
            let word: String
            switch action {
            case let .touch(_, _, phase, delay): word = "touch \(phase)" + (delay.map { " \($0)ms" } ?? "")
            case let .button(name, _): word = "button \(name)"
            case let .text(text): word = "text \(text)"
            case let .key(key, _, _): word = "key \(key)"
            default: word = "other"
            }
            if word == "touch move", words.last == word { continue }
            // Typing arrives a few letters per batch; show it as typed.
            if case let .text(text) = action, let last = words.last, last.hasPrefix("text ") {
                words[words.count - 1] = last + text
                continue
            }
            words.append(word)
        }
        return words.suffix(16).joined(separator: ", ")
    }
    #endif
}

#if WONDER_DIAGNOSTICS
/// A still home screen standing in for the simulator's picture in fixtures.
private struct SimulatorFixtureBackdrop: View {
    /// The hinge angle and screen swap the person asked for, drawn so a scenario can see them.
    let hinge: Int?
    let flipped: Bool

    var body: some View {
        GeometryReader { proxy in
            let side = proxy.size.width / 6
            ZStack(alignment: .top) {
                LinearGradient(colors: flipped ? [Color(red: 0.42, green: 0.22, blue: 0.55), Color(red: 0.16, green: 0.32, blue: 0.62)]
                                               : [Color(red: 0.16, green: 0.32, blue: 0.62), Color(red: 0.42, green: 0.22, blue: 0.55)],
                               startPoint: .top, endPoint: .bottom)
                if let hinge {
                    Text("Hinge \(hinge)°" + (flipped ? " · swapped" : ""))
                        .font(.caption.weight(.semibold)).foregroundStyle(.white)
                        .padding(.top, 6)
                } else if flipped {
                    Text("Swapped").font(.caption.weight(.semibold)).foregroundStyle(.white).padding(.top, 6)
                }
                Grid(horizontalSpacing: side / 2, verticalSpacing: side / 2) {
                    ForEach(0..<5, id: \.self) { _ in
                        GridRow {
                            ForEach(0..<4, id: \.self) { _ in
                                RoundedRectangle(cornerRadius: side / 4, style: .continuous)
                                    .fill(.white.opacity(0.35)).frame(width: side, height: side)
                            }
                        }
                    }
                }
                .padding(.top, proxy.size.height * 0.1)
            }
        }
        .accessibilityHidden(true)
    }
}
#endif

/// UIKit owns the finger: one touch is tracked from down to up and streamed
/// as simulator touches; the element is also the screen for VoiceOver, with
/// direct touch and actions for tap, long press, swipes and Home.
private struct SimulatorTouchSurface: UIViewRepresentable {
    let geometry: SimulatorScreenGeometry
    let name: String
    let state: String
    let controlling: Bool
    let canTakeControl: Bool
    let touch: (ComputerInputAction) -> Void
    let gesture: ([ComputerInputAction]) -> Void
    let home: () -> Void
    let takeControl: () -> Void

    func makeUIView(context: Context) -> Surface { Surface() }

    func updateUIView(_ view: Surface, context: Context) {
        view.geometry = geometry
        view.touch = touch
        view.gesture = gesture
        view.home = home
        view.takeControl = takeControl
        view.update(name: name, state: state, controlling: controlling, canTakeControl: canTakeControl)
    }

    static func dismantleUIView(_ view: Surface, coordinator: ()) {
        view.cancelTouch()
        view.touch = { _ in }
        view.gesture = { _ in }
    }

    final class Surface: UIView {
        var geometry: SimulatorScreenGeometry?
        var touch: (ComputerInputAction) -> Void = { _ in }
        var gesture: ([ComputerInputAction]) -> Void = { _ in }
        var home: () -> Void = {}
        var takeControl: () -> Void = {}
        private var controlling = false
        private var canTakeControl = false
        private var tracker = SimulatorTouchTracker()
        private weak var tracked: UITouch?

        init() {
            super.init(frame: .zero)
            isMultipleTouchEnabled = false
            backgroundColor = .clear
            isAccessibilityElement = true
            accessibilityIdentifier = "simulator-screen"
        }
        required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

        func update(name: String, state: String, controlling: Bool, canTakeControl: Bool) {
            if !controlling { cancelTouch() }
            let label = "\(name) screen"
            if accessibilityLabel != label { accessibilityLabel = label }
            if accessibilityValue != state { accessibilityValue = state }
            guard self.controlling != controlling || self.canTakeControl != canTakeControl
                    || accessibilityCustomActions == nil else { return }
            self.controlling = controlling
            self.canTakeControl = canTakeControl
            accessibilityTraits = controlling ? [.allowsDirectInteraction, .adjustable] : []
            accessibilityHint = controlling
                ? "Touch to tap and swipe, or use actions. Swipe up or down to scroll."
                : (canTakeControl ? "Take control to use this simulator." : nil)
            if controlling {
                let center = CGPoint(x: 0.5, y: 0.5)
                accessibilityCustomActions = [
                    action("Tap") { $0.gesture(SimulatorGesture.tap(at: center)) },
                    action("Long press") { $0.gesture(SimulatorGesture.longPress(at: center)) },
                    action("Swipe up") { $0.gesture(SimulatorGesture.swipe(.up)) },
                    action("Swipe down") { $0.gesture(SimulatorGesture.swipe(.down)) },
                    action("Swipe left") { $0.gesture(SimulatorGesture.swipe(.left)) },
                    action("Swipe right") { $0.gesture(SimulatorGesture.swipe(.right)) },
                    action("Home") { $0.home() },
                ]
            } else {
                accessibilityCustomActions = canTakeControl ? [action("Take control") { $0.takeControl() }] : []
            }
        }

        private func action(_ name: String, _ perform: @escaping (Surface) -> Void) -> UIAccessibilityCustomAction {
            UIAccessibilityCustomAction(name: name) { [weak self] _ in
                guard let self else { return false }
                perform(self)
                return true
            }
        }

        override func accessibilityActivate() -> Bool {
            guard controlling else { return false }
            gesture(SimulatorGesture.tap(at: CGPoint(x: 0.5, y: 0.5)))
            return true
        }
        override func accessibilityIncrement() { if controlling { gesture(SimulatorGesture.swipe(.up)) } }
        override func accessibilityDecrement() { if controlling { gesture(SimulatorGesture.swipe(.down)) } }

        private func devicePoint(_ touch: UITouch, clamped: Bool) -> CGPoint? {
            guard let geometry else { return nil }
            let local = touch.location(in: self)
            // This view is the screen's rectangle; the geometry is the viewport's.
            return geometry.devicePoint(CGPoint(x: local.x + geometry.screen.minX, y: local.y + geometry.screen.minY),
                                        clamped: clamped)
        }

        override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
            guard controlling, tracked == nil, let first = touches.first,
                  let action = tracker.began(at: devicePoint(first, clamped: false), time: first.timestamp) else { return }
            tracked = first
            touch(action)
        }
        override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
            guard let tracked, touches.contains(tracked),
                  let action = tracker.moved(to: devicePoint(tracked, clamped: true)) else { return }
            touch(action)
        }
        override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
            guard let tracked, touches.contains(tracked) else { return }
            self.tracked = nil
            if let action = tracker.ended(at: devicePoint(tracked, clamped: true), time: tracked.timestamp) { touch(action) }
        }
        override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
            guard let tracked, touches.contains(tracked) else { return }
            self.tracked = nil
            if let action = tracker.ended(at: nil, time: tracked.timestamp) { touch(action) }
        }

        /// Lifts a finger that is down when control ends or the view goes away.
        func cancelTouch() {
            guard tracker.isTouching else { tracked = nil; return }
            tracked = nil
            if let action = tracker.ended(at: nil, time: ProcessInfo.processInfo.systemUptime) { touch(action) }
        }
    }
}

/// A Duo simulator's hinge and screens, driven through the computer the same
/// way `scripts/wonderctl hinge` and `orient` drive them. The hinge angle
/// isn't readable from the simulator, so it is what this device last asked for.
@MainActor final class SimulatorPosture: ObservableObject {
    @Published private(set) var hinge: Int?
    /// Which way up the screens are; unknown until this device sets it, since the simulator can't be asked.
    @Published private(set) var flipped: Bool?
    @Published private(set) var busy = false
    @Published private(set) var message: String?
    private let choice: SimulatorChoice

    init(choice: SimulatorChoice) { self.choice = choice }

    private var isFixture: Bool {
        #if WONDER_DIAGNOSTICS
        ProcessInfo.processInfo.arguments.contains("-diagnostics-computer-simulator-fixture")
        #else
        false
        #endif
    }

    func setHinge(_ degrees: Int) { send("hinge", ["degrees": degrees]) { self.hinge = degrees } }

    /// Turning the Duo half a turn swaps which screen is which. Each choice names
    /// its end state, so it is right whichever way up the simulator started.
    func setScreens(swapped: Bool) {
        send("orientation", ["orientation": swapped ? "pud" : "portrait"]) { self.flipped = swapped }
    }

    private func send<Value: Encodable & Sendable>(_ route: String, _ body: [String: Value], applied: @escaping () -> Void) {
        guard !busy else { return }
        busy = true
        message = nil
        Task {
            defer { busy = false }
            do {
                #if WONDER_DIAGNOSTICS
                if isFixture {
                    try await Task.sleep(for: .milliseconds(300))
                    if ProcessInfo.processInfo.arguments.contains("-diagnostics-simulator-hinge-fails") {
                        throw PairingFailure.hostMessage(502, "The simulator’s hinge couldn’t be moved. Try again.")
                    }
                    applied()
                    return
                }
                #endif
                let _: Acknowledgement = try await choice.model.manage("/api/v1/simulators/\(choice.udid)/\(route)", method: "POST",
                                                                         body: try JSONEncoder().encode(body))
                applied()
            } catch is CancellationError {
            } catch {
                message = Self.message(error)
            }
        }
    }

    private struct Acknowledgement: Decodable, Sendable { let udid: String }

    private static func message(_ error: Error) -> String {
        switch error {
        case PairingFailure.hostMessage(_, let message): message
        case PairingFailure.response(let status) where [404, 405].contains(status):
            "Update Cupola on your computer to move the hinge from here."
        case is URLError: "Your computer can’t be reached right now."
        default: "The simulator’s hinge couldn’t be moved. Try again."
        }
    }
}

/// Fold, tent and open presets and Swap screens, as one header menu.
struct SimulatorHingeMenu: View {
    @ObservedObject var posture: SimulatorPosture

    var body: some View {
        Menu {
            ForEach(Self.presets, id: \.degrees) { preset in
                Button {
                    posture.setHinge(preset.degrees)
                } label: {
                    Label(preset.title, systemImage: posture.hinge == preset.degrees ? "checkmark" : preset.symbol)
                }
                .accessibilityIdentifier("simulator-hinge-\(preset.degrees)")
            }
            Divider()
            Button { posture.setScreens(swapped: false) } label: {
                Label("Screens upright", systemImage: posture.flipped == false ? "checkmark" : "rectangle.portrait")
            }
            .accessibilityIdentifier("simulator-upright-screens")
            Button { posture.setScreens(swapped: true) } label: {
                Label("Swap screens", systemImage: posture.flipped == true ? "checkmark" : "arrow.left.arrow.right")
            }
            .accessibilityIdentifier("simulator-swap-screens")
        } label: {
            Image(systemName: "rectangle.split.2x1").frame(minWidth: 44, minHeight: 44).contentShape(Rectangle())
        }
        .disabled(posture.busy)
        .accessibilityLabel("Hinge")
        .accessibilityValue([posture.hinge.map { "\($0) degrees" }, posture.flipped == true ? "screens swapped" : nil]
            .compactMap { $0 }.joined(separator: ", "))
        .accessibilityHint("Fold, tent or open the simulator, or swap its screens")
        .accessibilityIdentifier("simulator-hinge-menu")
    }

    private static let presets: [(title: String, degrees: Int, symbol: String)] = [
        ("Fold", 0, "book.closed"), ("Tent 90°", 90, "triangle"), ("Tent 135°", 135, "triangle"), ("Open", 180, "book"),
    ]
}
