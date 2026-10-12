import AppKit
import SwiftUI

/// The app's own icon (AppIcon.icns in the bundle), with the template's transparent margin trimmed.
struct AppIconView: View {
    var size: CGFloat

    var body: some View {
        Image(nsImage: NSApp.applicationIconImage ?? NSImage())
            .resizable()
            .interpolation(.high)
            .frame(width: size * 1.22, height: size * 1.22)
            .frame(width: size, height: size)
    }
}
