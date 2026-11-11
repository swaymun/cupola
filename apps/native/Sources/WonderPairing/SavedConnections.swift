import Foundation

/// Stored as one Keychain value. An empty filter means all computers, including new ones.
public struct SavedConnections: Codable, Sendable {
    public private(set) var connections: [SavedConnection]
    public private(set) var selectedHostIDs: Set<String>

    public init(legacy: SavedConnection? = nil) {
        connections = legacy.map { [$0] } ?? []
        selectedHostIDs = []
    }

    // A former per-device nickname (`connectionNames`) is ignored: the host
    // owns the computer's name now.
    private enum CodingKeys: String, CodingKey { case connections, selectedHostIDs }
    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        connections = try values.decode([SavedConnection].self, forKey: .connections)
        selectedHostIDs = try values.decodeIfPresent(Set<String>.self, forKey: .selectedHostIDs) ?? []
        selectedHostIDs.formIntersection(connections.map { $0.credential.hostInstallationId })
        if selectedHostIDs.count > 1 { selectedHostIDs = [] }
    }

    public func includes(_ host: String) -> Bool { selectedHostIDs.isEmpty || selectedHostIDs.contains(host) }
    public mutating func showAll() { selectedHostIDs = [] }

    public mutating func select(_ host: String) throws {
        guard connections.contains(where: { $0.credential.hostInstallationId == host }) else { throw PairingFailure.wrongHost }
        selectedHostIDs = [host]
    }

    public mutating func save(_ connection: SavedConnection) {
        let host = connection.credential.hostInstallationId
        if let index = connections.firstIndex(where: { $0.credential.hostInstallationId == host }) {
            connections[index] = connection.preservingStorage(from: connections[index])
        } else { connections.append(connection) }
    }

    /// The name its host reports (the host owns it), else its address.
    public func name(for connection: SavedConnection) -> String {
        connection.hostName ?? URL(string: connection.origin)?.host ?? "Computer"
    }

    public mutating func requirePairing() {
        for index in connections.indices { connections[index].requiresPairing = true }
    }

    public mutating func remove(_ host: String) {
        connections.removeAll { $0.credential.hostInstallationId == host }
        selectedHostIDs.remove(host)
    }

    public mutating func removeRoute(origin: String, from host: String) {
        guard let index = connections.firstIndex(where: { $0.credential.hostInstallationId == host }) else { return }
        if let remaining = connections[index].removingRoute(origin: origin) { connections[index] = remaining }
        else { remove(host) }
    }
}
