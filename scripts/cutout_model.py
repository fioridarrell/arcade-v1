"""Turns MediaPipe's selfie segmenter (landscape, float16) into the small file the media layer's cutout reads
(crates/arcade-desktop/dist/cutout-model.bin, and the website's copy).

    python3 scripts/cutout_model.py selfie_segmenter_landscape.tflite cutout-model.bin

The model: https://storage.googleapis.com/mediapipe-models/image_segmenter/selfie_segmenter_landscape/float16/latest/selfie_segmenter_landscape.tflite
(Apache-2.0, Google; the model card is linked from https://ai.google.dev/edge/mediapipe/solutions/vision/image_segmenter).
Needs `pip install tflite numpy` (the flatbuffer reader only, no TensorFlow).

The weights stay float16, as published. What changes is only how they are laid out: one list of steps in running
order, an activation folded into the convolution before it, and every weight once, so cutout.js needs no flatbuffer
reader. The file: "ACUT", u32 header length (little endian), the header (JSON), zeros to a multiple of 4, the
float16 weights. Offsets in the header count float16 values."""
import json, struct, sys
import numpy as np
import tflite

ACT = {"RELU": "relu", "HARD_SWISH": "hswish", "LOGISTIC": "sigmoid"}


def main(src, dst):
    buf = open(src, "rb").read()
    m = tflite.Model.GetRootAsModel(buf, 0)
    g = m.Subgraphs(0)
    names = {v: k for k, v in tflite.BuiltinOperator.__dict__.items() if not k.startswith("_")}

    def shape(t):
        return [int(x) for x in g.Tensors(t).ShapeAsNumpy()]

    def const(t):
        b = m.Buffers(g.Tensors(t).Buffer())
        if b.DataLength() == 0:
            return None
        kind = {tflite.TensorType.FLOAT16: np.float16, tflite.TensorType.FLOAT32: np.float32,
                tflite.TensorType.INT32: np.int32}[g.Tensors(t).Type()]
        return np.frombuffer(b.DataAsNumpy().tobytes(), dtype=kind).reshape(shape(t))

    ops, consts, users = [], {}, {}
    for i in range(g.OperatorsLength()):
        op = g.Operators(i)
        oc = m.OperatorCodes(op.OpcodeIndex())
        code = max(oc.BuiltinCode(), oc.DeprecatedBuiltinCode())
        name = names.get(code, code)
        ins = [op.Inputs(j) for j in range(op.InputsLength())]
        out = op.Outputs(0)
        if name == "DEQUANTIZE":                          # float16 constant -> its float32 tensor: kept as float16
            consts[out] = const(ins[0])
            continue
        if name == "CUSTOM":
            name = "CUSTOM:" + oc.CustomCode().decode()
        ops.append((name, ins, out, op))
        for t in ins:
            users[t] = users.get(t, 0) + 1

    def value(t):
        return consts[t] if t in consts else const(t)

    blob, steps, outputs = [], [], g.Outputs(0)
    at = [0]

    def put(a):
        a = np.ascontiguousarray(a, dtype=np.float16).ravel()
        off = at[0]
        blob.append(a)
        at[0] += a.size
        return off

    by_out = {}
    for name, ins, out, op in ops:
        o = op.BuiltinOptions()
        if name in ("CONV_2D", "DEPTHWISE_CONV_2D"):
            c = tflite.Conv2DOptions() if name == "CONV_2D" else tflite.DepthwiseConv2DOptions()
            c.Init(o.Bytes, o.Pos)
            assert c.FusedActivationFunction() == 0 and c.DilationHFactor() == 1 and c.StrideH() == c.StrideW()
            w, b = value(ins[1]), value(ins[2])
            step = {"op": "conv" if name == "CONV_2D" else "dw", "in": ins[0], "out": out, "kh": w.shape[1], "kw": w.shape[2],
                    "s": c.StrideH(), "same": c.Padding() == tflite.Padding.SAME, "cin": shape(ins[0])[3], "cout": shape(out)[3],
                    "w": put(w), "b": put(b)}     # conv weights [out, kh, kw, in]; depthwise [1, kh, kw, ch]
        elif name in ACT:
            prev = by_out.get(ins[0])
            if prev is not None and prev["op"] in ("conv", "dw") and "act" not in prev and users.get(ins[0]) == 1 and ins[0] != outputs:
                prev["act"] = ACT[name]
                by_out[out] = prev
                del by_out[prev["out"]]
                prev["out"] = out
                continue
            step = {"op": ACT[name], "in": ins[0], "out": out}
        elif name in ("ADD", "MUL"):
            step = {"op": name.lower(), "in": ins[0], "in2": ins[1], "out": out}
            assert shape(ins[0]) == shape(out)
        elif name in ("MEAN", "AVERAGE_POOL_2D"):          # over the whole picture, per channel
            assert shape(out)[1:3] == [1, 1]
            step = {"op": "mean", "in": ins[0], "out": out}
        elif name == "RESIZE_BILINEAR":
            c = tflite.ResizeBilinearOptions()
            c.Init(o.Bytes, o.Pos)
            assert c.HalfPixelCenters() and not c.AlignCorners()
            step = {"op": "resize", "in": ins[0], "out": out}
        elif name == "CUSTOM:Convolution2DTransposeBias":
            pad, sw, sh = struct.unpack("<iii", bytes(op.CustomOptionsAsNumpy()))
            w, b = value(ins[1]), value(ins[2])
            assert sw == sh == 2 and tuple(w.shape[1:3]) == (2, 2)   # kernel 2, stride 2: no overlap, no padding
            step = {"op": "tconv", "in": ins[0], "out": out, "cin": shape(ins[0])[3], "cout": w.shape[0], "w": put(w), "b": put(b)}
        else:
            raise SystemExit("an op cutout.js does not run: " + name)
        steps.append(step)
        by_out[out] = step

    # Tensors renumbered from 0 in running order, with their shapes [h, w, c].
    ids = {g.Inputs(0): 0}
    for s in steps:
        for k in ("in", "in2", "out"):
            if k in s and s[k] not in ids:
                ids[s[k]] = len(ids)
    tensors = [None] * len(ids)
    for t, n in ids.items():
        tensors[n] = shape(t)[1:]
    for s in steps:
        for k in ("in", "in2", "out"):
            if k in s:
                s[k] = ids[s[k]]
    head = {"model": "selfie_segmenter_landscape (MediaPipe, Apache-2.0)", "input": ids[g.Inputs(0)], "output": ids[outputs],
            "tensors": tensors, "steps": steps}
    h = json.dumps(head, separators=(",", ":")).encode()
    with open(dst, "wb") as f:
        f.write(b"ACUT" + struct.pack("<I", len(h)) + h)
        f.write(b"\0" * (-(8 + len(h)) % 4))
        for a in blob:
            f.write(a.tobytes())
    print(f"{len(steps)} steps, {at[0]} weights, {8 + len(h) + at[0] * 2} bytes")


if __name__ == "__main__":
    main(*sys.argv[1:3])
