# Bundled primary terminal font

Unmodified JetBrains Mono 2.304, downloaded from the upstream `v2.304` tag:

`https://github.com/JetBrains/JetBrainsMono/tree/v2.304/fonts/ttf`

Copyright 2020 The JetBrains Mono Project Authors. Licensed under SIL OFL 1.1;
see `OFL.txt`. The client embeds regular, bold, italic and bold-italic faces and
registers them before GPUI resolves any family. No host font installation,
network fetch at runtime, or package-manager dependency is required.

SHA-256:

```text
a0bf60ef0f83c5ed4d7a75d45838548b1f6873372dfac88f71804491898d138f  JetBrainsMono-Regular.ttf
5590990c82e097397517f275f430af4546e1c45cff408bde4255dad142479dcb  JetBrainsMono-Bold.ttf
9d0a1f7a708e6af183f1193b7e81d40da294f5c67682c085d8401c60aac8ded4  JetBrainsMono-Italic.ttf
4039d5ce0ed225bf9c8b2c8c6436290ae2f356b7e90d70fa666227238324aa3b  JetBrainsMono-BoldItalic.ttf
```

When updating, retain upstream license/copyright notices, update the checksums,
and run the `fonts::tests` client suite plus a native clean-font visual launch.
