// The app's code, compiled against the public API's android.jar from the AOSP build
// (scripts/phone/aosp.sh, -PandroidJar=<path>); nothing here is from the Android SDK.
// build-apk.sh turns the classes into the APK with the AOSP build's aapt2, d8, zipalign and
// apksigner.
plugins {
    id("org.jetbrains.kotlin.jvm")
}
val androidJar = providers.gradleProperty("androidJar").orNull
    ?: throw GradleException("the app compiles against the AOSP build's android.jar: pass -PandroidJar=<path>")
kotlin {
    jvmToolchain(17)
    // java.* and android.* come from android.jar, as on the phone, not from the JDK
    compilerOptions { noJdk.set(true) }
}
dependencies {
    compileOnly(files(androidJar))
    implementation(project(":scene"))
}
// What d8 dexes: the app's classes and every runtime jar (the scene, Kotlin's standard library).
tasks.register<Sync>("dexInputs") {
    from(tasks.named("jar"))
    from(configurations.named("runtimeClasspath"))
    into(layout.buildDirectory.dir("dex-inputs"))
}
