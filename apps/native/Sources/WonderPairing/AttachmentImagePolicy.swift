import Foundation
import ImageIO
import UniformTypeIdentifiers

/// The size rule every image attachment follows: the app's Photos, paste and
/// Camera, and the share extension. An image within `maximumBytes` is sent
/// as it is. A larger one is made smaller only when it is a still, opaque
/// JPEG or HEIC photo; a larger PNG, transparent or animated image is refused
/// rather than silently flattened to JPEG.
///
/// Privacy is a separate choice and is not decided here: an original keeps its
/// metadata (including any location), and a re-encoded JPEG carries none.
public enum AttachmentImagePolicy {
    public static let maximumBytes = 8 * 1024 * 1024

    public static func mayShrink(_ source: CGImageSource) -> Bool {
        guard let identifier = CGImageSourceGetType(source), let type = UTType(identifier as String),
              [UTType.jpeg, .heic, .heif].contains(where: { type.conforms(to: $0) }),
              CGImageSourceGetCount(source) == 1 else { return false }
        let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any]
        return (properties?[kCGImagePropertyHasAlpha] as? NSNumber)?.boolValue != true
    }
}
