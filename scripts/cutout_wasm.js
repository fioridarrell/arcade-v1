// Builds scripts/cutout_kernels.wat into the KERNELS line of crates/arcade-desktop/dist/cutout.js (base64), so the
// cutout has no second file to fetch. Needs wabt (Apache-2.0) from npm, only here, never in the app:
//
//     npm pack wabt && tar xzf wabt-*.tgz && node scripts/cutout_wasm.js ./package
//
// Then copy cutout.js to the website (arcade-src/arcade/web/cutout/): the two are one file.
const fs = require("fs"), path = require("path");
const wabt = require(path.resolve(process.argv[2] || "wabt"));
const here = path.join(__dirname, ".."), js = path.join(here, "crates/arcade-desktop/dist/cutout.js");
wabt().then((w) => {
  const mod = w.parseWat("cutout_kernels.wat", fs.readFileSync(path.join(__dirname, "cutout_kernels.wat"), "utf8"), { simd: true });
  mod.validate();
  const bin = Buffer.from(mod.toBinary({}).buffer);
  const text = fs.readFileSync(js, "utf8"), line = /^  const KERNELS = "[^"]*";$/m;
  if (!line.test(text)) throw new Error("no KERNELS line in cutout.js");
  fs.writeFileSync(js, text.replace(line, '  const KERNELS = "' + bin.toString("base64") + '";'));
  console.log(bin.length + " bytes of WebAssembly in cutout.js");
});
