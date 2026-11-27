// The Android half: the library (:tinyrtc) and the app that proves it on a phone (:demo).
// The core's static library comes from ./build-core.sh (see tinyrtc/src/main/cpp/CMakeLists.txt).
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

rootProject.name = "tinyrtc-android"
include(":tinyrtc", ":demo")
