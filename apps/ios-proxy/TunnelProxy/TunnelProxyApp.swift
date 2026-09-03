import SwiftUI

@main
struct TunnelProxyApp: App {
    @StateObject private var model = TunnelAppModel()

    var body: some Scene {
        WindowGroup {
            WebHostView(model: model)
                .ignoresSafeArea(edges: .bottom)
                .task {
                    await model.autoConnectIfAsked()
                }
        }
    }
}
