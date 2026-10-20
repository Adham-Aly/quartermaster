// Reads the images on a macOS pasteboard into files `0`, `1`, … in the folder `argv[0]`.
// `argv[1]`, when given, names a private pasteboard to read instead of the general one.
// Run by `osascript -l JavaScript` from `clipboard_image.rs`; prints how it found them.
ObjC.import('AppKit');

function run(argv) {
  var dir = argv[0];
  var name = argv.length > 1 ? argv[1] : '';
  var pb = name ? $.NSPasteboard.pasteboardWithName(name) : $.NSPasteboard.generalPasteboard;
  var count = 0;
  function next() {
    return dir + '/' + count++;
  }
  // Anything AppKit can draw (TIFF, HEIC, PDF, …), as PNG.
  function asPng(image) {
    if (image.isNil()) return;
    var tiff = image.TIFFRepresentation;
    if (tiff.isNil()) return;
    var rep = $.NSBitmapImageRep.imageRepWithData(tiff);
    if (rep.isNil()) return;
    var png = rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $({}));
    if (!png.isNil()) png.writeToFileAtomically(next(), true);
  }

  // Files copied in the Finder: the files themselves, never their icons.
  var urls = pb.readObjectsForClassesOptions(
    $([$.NSURL]),
    $({ NSPasteboardURLReadingFileURLsOnlyKey: true })
  );
  if (!urls.isNil() && urls.count > 0) {
    var kept = ['png', 'jpg', 'jpeg', 'gif', 'webp'];
    for (var i = 0; i < urls.count; i++) {
      var url = urls.objectAtIndex(i);
      if (kept.indexOf(url.pathExtension.js.toLowerCase()) >= 0) {
        var data = $.NSData.dataWithContentsOfURL(url);
        if (!data.isNil()) data.writeToFileAtomically(next(), true);
      } else {
        asPng($.NSImage.alloc.initWithContentsOfURL(url));
      }
    }
    return 'files ' + count;
  }

  // Image data: the formats models read, as they are.
  var types = ['public.png', 'public.jpeg', 'com.compuserve.gif', 'org.webmproject.webp'];
  for (var j = 0; j < types.length; j++) {
    var found = pb.dataForType(types[j]);
    if (!found.isNil()) {
      found.writeToFileAtomically(next(), true);
      return 'data ' + count;
    }
  }
  asPng($.NSImage.alloc.initWithPasteboard(pb));
  return 'image ' + count;
}
