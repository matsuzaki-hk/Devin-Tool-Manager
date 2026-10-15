import zipfile, os, sys

src = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "extension")
out = sys.argv[1]

content_types = """<?xml version="1.0" encoding="utf-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="json" ContentType="application/json"/>
  <Default Extension="js" ContentType="application/javascript"/>
  <Default Extension="css" ContentType="text/css"/>
  <Default Extension="svg" ContentType="image/svg+xml"/>
  <Default Extension="vsixmanifest" ContentType="text/xml"/>
</Types>
"""

with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("[Content_Types].xml", content_types)
    z.write(os.path.join(src, "vsixmanifest.xml"), "extension.vsixmanifest")
    for root, _, fs in os.walk(src):
        for f in fs:
            if f == "vsixmanifest.xml":
                continue
            p = os.path.join(root, f)
            arc = "extension/" + os.path.relpath(p, src).replace("\\", "/")
            z.write(p, arc)
print("wrote", out)
