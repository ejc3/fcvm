plugins {
    id("org.jetbrains.kotlin.jvm")
}
kotlin {
    jvmToolchain(17)
}
dependencies {
    testImplementation("junit:junit:4.13.2")
}
tasks.test {
    // The shader check compiles app/src/main/assets/shaders with glslangValidator.
    systemProperty("shaderDir", rootProject.file("app/src/main/assets/shaders").absolutePath)
    // Two contracts of the app's render thread are pinned against its source.
    systemProperty("appSrcDir", rootProject.file("app/src/main/kotlin/dev/fcvm/robotpark").absolutePath)
    testLogging { events("passed", "failed"); showStandardStreams = true; exceptionFormat = org.gradle.api.tasks.testing.logging.TestExceptionFormat.FULL }
}
