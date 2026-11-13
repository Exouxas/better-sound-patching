# Models

`dpdfnet2_48khz_hr.onnx` is the DPDFNet-2 speech enhancement model (48 kHz) by Ceva, from
<https://huggingface.co/Ceva-IP/DPDFNet> at revision `dd6818d00f50c836fed43a6243ebe49116de5964`,
file `onnx/dpdfnet2_48khz_hr.onnx`, unchanged. It is licensed under the Apache License 2.0, see
`LICENSE-DPDFNet` (from <https://github.com/ceva-ip/DPDFNet>). Paper: arXiv:2512.16420.

SHA-256: `7f0575a5cec0ba4ffd8f8bd657e06d007e4ccdd955d76faab922b9d3291dc14b`

It is embedded in the program (`src/dpdfnet.rs`) and run with [tract](https://github.com/sonos/tract).
