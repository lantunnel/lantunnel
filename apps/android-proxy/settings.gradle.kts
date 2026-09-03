pluginManagement {
    repositories {
        maven {
            name = "GoogleAndroid"
            url = uri("https://dl.google.com/dl/android/maven2/")
        }
        google()
        mavenCentral()
        // Regional fallback, tried last. Reorder it above mavenCentral only if
        // your network needs it.
        maven {
            name = "AliyunPublic"
            url = uri("https://maven.aliyun.com/repository/public")
        }
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        maven {
            name = "GoogleAndroid"
            url = uri("https://dl.google.com/dl/android/maven2/")
        }
        google()
        mavenCentral()
        // Regional fallback, tried last. Reorder it above mavenCentral only if
        // your network needs it.
        maven {
            name = "AliyunPublic"
            url = uri("https://maven.aliyun.com/repository/public")
        }
    }
}

rootProject.name = "LantunnelAndroid"
include(":app")
