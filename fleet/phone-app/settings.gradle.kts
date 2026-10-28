// Robot Park, the phone app (docs/fleet/ARCHITECTURE.md, "Phones mode"). `scene` is plain
// Kotlin (the scene model, the control protocol, the tile geometry) and its tests run on any
// JVM; `app` is the Android app around it and needs the Android SDK. -PsceneOnly=true leaves
// the app out, so the scene's tests run where no SDK is installed.
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}
rootProject.name = "robotpark"
include(":scene")
if (providers.gradleProperty("sceneOnly").orNull != "true") include(":app")
