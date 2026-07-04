#!/usr/bin/env python3
# Patch a tauri-generated android app/build.gradle.kts to sign the release build
# from gen/android/keystore.properties. Idempotent.
import sys

p = sys.argv[1]
s = open(p, encoding="utf-8").read()

if "signingConfigs" not in s:
    block = '''
    signingConfigs {
        create("release") {
            val kp = Properties()
            val kpf = rootProject.file("keystore.properties")
            if (kpf.exists()) {
                kpf.inputStream().use { kp.load(it) }
                keyAlias = kp.getProperty("keyAlias")
                keyPassword = kp.getProperty("keyPassword")
                storeFile = file(kp.getProperty("storeFile"))
                storePassword = kp.getProperty("storePassword")
            }
        }
    }
'''
    s = s.replace("android {\n", "android {\n" + block, 1)

if "signingConfig = signingConfigs.getByName(\"release\")" not in s:
    s = s.replace(
        'getByName("release") {\n',
        'getByName("release") {\n            signingConfig = signingConfigs.getByName("release")\n',
        1,
    )

open(p, "w", encoding="utf-8").write(s)
print("patched", p)
