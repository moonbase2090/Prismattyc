import AppKit
import CoreGraphics

// Compose a macOS app tile: transparent 1024 canvas, a 704px rounded body
// (160px margin so circular Dock/preview masks do not shear the sides),
// brand-dark fill, source art inside. Args: <src.png> <out.png>
let args = CommandLine.arguments
guard args.count == 3 else { fatalError("usage: make_tile <src.png> <out.png>") }
let srcPath = args[1], outPath = args[2]

let side = 1024
// 824 filled the canvas so tightly that a circular Dock/preview
// mask sheared the top and right. 736 leaves 144px margin.
let body = 736.0
let origin = (Double(side) - body) / 2.0
let radius = body * 0.18

guard let srcImg = NSImage(contentsOfFile: srcPath),
      let srcCG = srcImg.cgImage(forProposedRect: nil, context: nil, hints: nil)
else { fatalError("cannot read \(srcPath)") }

let cs = CGColorSpaceCreateDeviceRGB()
guard let ctx = CGContext(data: nil, width: side, height: side,
                          bitsPerComponent: 8, bytesPerRow: 0, space: cs,
                          bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
else { fatalError("no context") }

ctx.interpolationQuality = .high
ctx.clear(CGRect(x: 0, y: 0, width: side, height: side))

let bodyRect = CGRect(x: origin, y: origin, width: body, height: body)
let path = CGPath(roundedRect: bodyRect, cornerWidth: radius, cornerHeight: radius, transform: nil)

// Fill body with brand dark (#121214), then clip and draw the art over it.
ctx.addPath(path)
ctx.setFillColor(CGColor(red: 0x12/255.0, green: 0x12/255.0, blue: 0x14/255.0, alpha: 1))
ctx.fillPath()

ctx.addPath(path)
ctx.clip()
ctx.draw(srcCG, in: bodyRect)

guard let out = ctx.makeImage() else { fatalError("no image") }
let rep = NSBitmapImageRep(cgImage: out)
guard let png = rep.representation(using: .png, properties: [:]) else { fatalError("no png") }
try! png.write(to: URL(fileURLWithPath: outPath))
print("wrote \(outPath)")
