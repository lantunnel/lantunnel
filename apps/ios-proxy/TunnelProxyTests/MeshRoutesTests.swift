import XCTest
@testable import TunnelProxy

/// The runtime lives in the PacketTunnel extension, so everything the app
/// knows about the mesh arrives as the provider's status JSON — where the
/// runtime's own object is nested under `native_status`. Reading the root
/// found nothing and showed an empty Tunnel on a working connection.
@MainActor
final class MeshRoutesTests: XCTestCase {
    private let providerShape = """
    {
      "running": true,
      "native_status": {
        "peer_directory": {
          "peers": [
            {
              "peer_id": "peer-a",
              "overlay_ip": "198.18.0.7",
              "phase": "connected",
              "current_path": "direct",
              "exports": [{"prefix": "192.168.7.0/24"}, {"prefix": "10.2.0.0/16"}]
            },
            {
              "peer_id": "peer-b",
              "overlay_ip": "198.18.0.9",
              "phase": "connected",
              "current_path": "encrypted_relay",
              "exports": [{"prefix": "192.168.7.0/24"}]
            }
          ]
        }
      }
    }
    """

    func testKnownPeersReadsThroughNativeStatus() {
        let peers = TunnelAppModel.knownPeers(from: providerShape)

        XCTAssertEqual(peers.count, 2)
        XCTAssertEqual(peers.first?.peerId, "peer-a")
        XCTAssertEqual(peers.first?.path, "Direct")
        XCTAssertEqual(peers.last?.path, "Relay")
    }

    /// The FFI hands back the same object unwrapped; both shapes must read.
    func testKnownPeersStillReadsAnUnwrappedRoot() {
        let bare = """
        {"peer_directory":{"peers":[{"peer_id":"solo","exports":[]}]}}
        """

        XCTAssertEqual(TunnelAppModel.knownPeers(from: bare).count, 1)
    }

    func testRoutesFromExportsCollectsEveryPublishedPrefixOnce() {
        let routes = MobileConfig.routesFromExports(providerShape)

        XCTAssertEqual(routes, ["10.2.0.0/16", "192.168.7.0/24", "198.18.0.0/16"])
    }

    /// Peer addresses live on the overlay, so it is routed whatever else is —
    /// including when nothing is published and when the status is unreadable.
    func testRoutesFromExportsAlwaysCarriesTheOverlay() {
        XCTAssertEqual(MobileConfig.routesFromExports("{}"), ["198.18.0.0/16"])
        XCTAssertEqual(MobileConfig.routesFromExports("not json"), ["198.18.0.0/16"])
    }

    /// The mesh view has to travel with the status poll.
    ///
    /// The app process has no runtime — the extension does — so a bridge
    /// built here answers for something that was never started. The provider
    /// status is the only place the app can learn what the Tunnel holds.
    func testSnapshotCarriesTheProviderStatusForward() {
        let snapshot = TunnelControlService.makeSnapshot(
            for: .connected,
            routeCount: 1,
            providerStatusJSON: providerShape
        )

        XCTAssertEqual(
            TunnelAppModel.knownPeers(from: snapshot.providerStatusJSON ?? "").count,
            2
        )
    }

    /// There is no opting out of following the mesh. Which of two overlapping
    /// prefixes wins is Tunnel First's question, decided in the engine, not a
    /// reason to refuse to learn the remote one.
    func testTheMeshIsAlwaysFollowed() {
        let model = TunnelAppModel(controlService: nil)
        model.config.lanRoutes = ["172.31.0.0/16"]
        model.latestProviderStatusJSON = providerShape

        XCTAssertEqual(
            model.effectiveLanRoutes(),
            ["10.2.0.0/16", "192.168.7.0/24", "198.18.0.0/16"]
        )
    }

    /// The VPN's route set is fixed when the tunnel starts, so a prefix a Peer
    /// published afterwards is listed but not carried until the next
    /// reconnect. Saying so beats a list that quietly disagrees.
    func testDerivedRoutesSummaryNamesWhatIsNotYetCarried() {
        let summary = TunnelAppModel.derivedRoutesSummary(
            following: true,
            derived: ["10.0.0.0/8", "198.18.0.0/16"],
            running: true,
            routedAtConnect: ["198.18.0.0/16"]
        )

        XCTAssertTrue(summary.contains("reconnect"))
    }

    func testDerivedRoutesSummaryIsQuietWhenTheTunnelAlreadyCarriesThem() {
        let routes = ["10.0.0.0/8", "198.18.0.0/16"]
        let summary = TunnelAppModel.derivedRoutesSummary(
            following: true,
            derived: routes,
            running: true,
            routedAtConnect: Set(routes)
        )

        XCTAssertFalse(summary.contains("reconnect"))
        XCTAssertTrue(summary.contains("10.0.0.0/8"))
    }

    func testDerivedRoutesSummarySaysWhenFollowingIsOff() {
        let summary = TunnelAppModel.derivedRoutesSummary(
            following: false,
            derived: ["198.18.0.0/16"],
            running: false,
            routedAtConnect: []
        )

        XCTAssertTrue(summary.lowercased().contains("off"))
    }

    /// The tunnel carries what the mesh publishes, never a hand-typed list.
    func testFollowingRoutesTheMeshAndLeavesTheManualListAlone() {
        let model = TunnelAppModel(controlService: nil)
        model.config.lanRoutes = ["172.31.0.0/16"]
        model.latestProviderStatusJSON = providerShape

        XCTAssertEqual(
            model.effectiveLanRoutes(),
            ["10.2.0.0/16", "192.168.7.0/24", "198.18.0.0/16"]
        )
        XCTAssertEqual(model.config.lanRoutes, ["172.31.0.0/16"])
    }

    /// An unknown start set is not a mismatch.
    ///
    /// The app can be relaunched while the tunnel keeps running, and what it
    /// started with is not in memory any more. Comparing against an empty set
    /// made every derived list look stale, so the hint never went away.
    func testAnUnknownStartSetIsNotClaimedStale() {
        let summary = TunnelAppModel.derivedRoutesSummary(
            following: true,
            derived: ["198.18.0.0/16"],
            running: true,
            routedAtConnect: []
        )

        XCTAssertFalse(summary.contains("reconnect"))
    }

    /// Connect happens while the runtime is stopped.
    ///
    /// The provider is not queried when disconnected, so deriving from the
    /// live status at the moment Connect is pressed yields only the overlay —
    /// following the mesh routed nothing any Peer publishes, which is the
    /// whole feature. What was last seen has to survive the disconnect.
    func testConnectRoutesTheMeshItLastSaw() {
        let model = TunnelAppModel(controlService: nil)
        model.rememberedMeshRoutes = ["192.168.7.0/24"]
        model.latestProviderStatusJSON = nil

        XCTAssertEqual(
            model.effectiveLanRoutes(),
            ["192.168.7.0/24", "198.18.0.0/16"]
        )
    }

    /// A Peer that has appeared since is added, not swapped in.
    func testConnectMergesWhatIsRememberedWithWhatIsLive() {
        XCTAssertEqual(
            TunnelAppModel.mergedMeshRoutes(
                remembered: ["192.168.7.0/24", "198.18.0.0/16"],
                derived: ["10.2.0.0/16", "198.18.0.0/16"]
            ),
            ["10.2.0.0/16", "192.168.7.0/24", "198.18.0.0/16"]
        )
    }

    /// A status the app could not fetch is not news that the mesh is empty.
    func testAMissingProviderStatusDoesNotForgetTheMesh() {
        let model = TunnelAppModel(controlService: nil)
        model.latestProviderStatusJSON = providerShape

        model.noteProviderStatus(nil)

        XCTAssertEqual(model.latestProviderStatusJSON, providerShape)
    }

    func testASeenMeshIsRemembered() {
        let model = TunnelAppModel(controlService: nil)
        model.statusPresentation = .connected(
            routeCount: 1,
            pathMode: "Direct",
            peer: "-",
            p2pTraffic: "-",
            relayTraffic: "-",
            uptime: "-",
            platformHeartbeat: "-",
            transportHeartbeat: "-"
        )

        model.noteProviderStatus(providerShape)

        XCTAssertEqual(
            model.rememberedMeshRoutes,
            ["10.2.0.0/16", "192.168.7.0/24", "198.18.0.0/16"]
        )
    }

    /// The runtime reports an empty peer directory while connecting and after
    /// a gateway resync. Recording those wiped the memory a moment after every
    /// Connect, so the feature had to re-earn it every time.
    func testAMeshSeenWhileNotConnectedIsNotRecorded() {
        let model = TunnelAppModel(controlService: nil)
        model.statusPresentation = .connecting(routeCount: 1)
        model.rememberedMeshRoutes = ["192.168.7.0/24"]

        model.noteProviderStatus(#"{"native_status":{"peer_directory":{"peers":[]}}}"#)

        XCTAssertEqual(model.rememberedMeshRoutes, ["192.168.7.0/24"])
    }

    /// Peer addresses live on the overlay, so it is carried whatever the mesh
    /// does or does not publish. Without it the tunnel reaches no Peer at all,
    /// and nothing on screen would say why.
    func testTheOverlayIsCarriedWhenNothingIsPublished() {
        let model = TunnelAppModel(controlService: nil)
        model.latestProviderStatusJSON = "{}"

        XCTAssertTrue(model.effectiveLanRoutes().contains(MobileConfig.overlayRoute))
    }

    /// A hand-typed list is not a route source any more: mergedMeshRoutes never
    /// consulted it, so leaving it populated must change nothing.
    func testAHandTypedListIsNotRouted() {
        let model = TunnelAppModel(controlService: nil)
        model.latestProviderStatusJSON = "{}"
        model.config.lanRoutes = ["172.31.0.0/16"]

        XCTAssertFalse(model.effectiveLanRoutes().contains("172.31.0.0/16"))
    }

    func testTheOverlayIsNotDuplicatedWhenAlreadyListed() {
        let model = TunnelAppModel(controlService: nil)
        model.config.lanRoutes = [MobileConfig.overlayRoute, "10.0.0.0/8"]

        XCTAssertEqual(
            model.effectiveLanRoutes().filter { $0 == MobileConfig.overlayRoute }.count,
            1
        )
    }
}
