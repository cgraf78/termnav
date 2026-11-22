// Native pasteboard and window probes for paste-image-gui.py on macOS.
import AppKit
import CoreGraphics
import ImageIO

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("paste-preview-macos: \(message)\n".utf8))
    exit(1)
}

/// Publish only the PNG flavor so Termnav exercises its image-only paste route.
func seedClipboard(_ path: URL) throws {
    let environment = ProcessInfo.processInfo.environment
    guard environment["GITHUB_ACTIONS"] == "true",
          environment["RUNNER_ENVIRONMENT"] == "github-hosted"
    else {
        fail("the native clipboard test requires an isolated GitHub-hosted runner")
    }
    let png = try Data(contentsOf: path)
    let clipboard = NSPasteboard.general
    clipboard.clearContents()
    guard clipboard.setData(png, forType: .png) else {
        fail("could not publish the PNG to the test runner's pasteboard")
    }
}

/// Identify the fixture window even if the spawned WezTerm is in the background.
func findWindow(pid: pid_t) {
    let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements],
                                            kCGNullWindowID) as? [[String: Any]] ?? []
    let ids = windows.compactMap { window -> UInt32? in
        guard window[kCGWindowOwnerPID as String] as? pid_t == pid,
              window[kCGWindowLayer as String] as? Int == 0
        else { return nil }
        return window[kCGWindowNumber as String] as? UInt32
    }
    guard ids.count <= 1 else { fail("WezTerm process \(pid) exposed multiple normal windows: \(ids)") }
    if let id = ids.first { print(id) }
}

/// Match the fixture's left-red/right-blue regions in color-managed window pixels.
func hasPreview(_ path: URL) -> Bool {
    guard let source = CGImageSourceCreateWithURL(path as CFURL, nil),
          let image = CGImageSourceCreateImageAtIndex(source, 0, nil),
          let srgb = CGColorSpace(name: CGColorSpace.sRGB)
    else { fail("could not decode screenshot at \(path.path)") }

    let stride = image.width * 4
    var pixels = [UInt8](repeating: 0, count: stride * image.height)
    return pixels.withUnsafeMutableBytes { bytes in
        let bitmap = CGBitmapInfo.byteOrder32Big.rawValue | CGImageAlphaInfo.premultipliedLast.rawValue
        guard let context = CGContext(data: bytes.baseAddress, width: image.width,
                                      height: image.height, bitsPerComponent: 8,
                                      bytesPerRow: stride, space: srgb, bitmapInfo: bitmap)
        else { fail("could not convert screenshot to RGBA") }
        context.draw(image, in: CGRect(x: 0, y: 0, width: image.width, height: image.height))

        var consecutive = 0
        for row in 0..<image.height {
            var reds = 0
            var blues = 0
            var lastRed = -1
            var firstBlue = image.width
            for column in 0..<image.width {
                let offset = row * stride + column * 4
                let (red, green, blue) = (bytes[offset], bytes[offset + 1], bytes[offset + 2])
                // Allow for color conversion and the GUI's native pixel density.
                if red >= 200 && green <= 70 && blue <= 70 {
                    reds += 1
                    lastRed = column
                }
                if blue >= 200 && green <= 70 && red <= 70 {
                    blues += 1
                    firstBlue = min(firstBlue, column)
                }
            }
            consecutive = reds >= 32 && blues >= 32 && lastRed < firstBlue ? consecutive + 1 : 0
            if consecutive >= 20 { return true }
        }
        return false
    }
}

guard CommandLine.arguments.count == 3 else {
    fail("usage: paste-preview-macos clipboard <png> | window <pid> | preview <png>")
}
let argument = CommandLine.arguments[2]
switch CommandLine.arguments[1] {
case "clipboard":
    do { try seedClipboard(URL(fileURLWithPath: argument)) }
    catch { fail("\(error.localizedDescription)") }
case "window":
    guard let pid = pid_t(argument), pid > 0 else { fail("invalid process ID: \(argument)") }
    findWindow(pid: pid)
case "preview":
    if hasPreview(URL(fileURLWithPath: argument)) { print("yes") }
default:
    fail("unknown command: \(CommandLine.arguments[1])")
}
