# libtinyrtc_jni.so = the JNI shim (tinyrtc_jni.c) + the core, linked in statically from the size
# build of core/ (android/build-core.sh copies it to android/tinyrtc/core-lib/<abi>/libtinyrtc.a).
# One library, one exported symbol (JNI_OnLoad registers the natives), and --gc-sections keeps only
# what the shim reaches.
cmake_minimum_required(VERSION 3.22.1)
project(tinyrtc_jni C)

get_filename_component(TINYRTC_REPO "${CMAKE_CURRENT_LIST_DIR}/../../../../.." ABSOLUTE)
get_filename_component(TINYRTC_CORE_LIB "${CMAKE_CURRENT_LIST_DIR}/../../../core-lib/${ANDROID_ABI}/libtinyrtc.a" ABSOLUTE)

if(NOT EXISTS "${TINYRTC_CORE_LIB}")
  message(FATAL_ERROR
    "tinyrtc: the core for ${ANDROID_ABI} is missing: ${TINYRTC_CORE_LIB}\n"
    "Build it first (both ABIs): ANDROID_NDK_HOME=<sdk>/ndk/27.0.12077973 android/build-core.sh")
endif()

add_library(tinyrtc_core STATIC IMPORTED)
set_target_properties(tinyrtc_core PROPERTIES IMPORTED_LOCATION "${TINYRTC_CORE_LIB}")

add_library(tinyrtc_jni SHARED tinyrtc_jni.c)
target_include_directories(tinyrtc_jni PRIVATE "${TINYRTC_REPO}/core/include")
target_compile_options(tinyrtc_jni PRIVATE
  -Os -fvisibility=hidden -ffunction-sections -fdata-sections -Wall -Wextra -Wno-unused-parameter)
# The core's own needs (std on Android): libc, libm, libdl; liblog for the shim's logging.
target_link_libraries(tinyrtc_jni PRIVATE tinyrtc_core log m dl)
target_link_options(tinyrtc_jni PRIVATE
  "-Wl,--version-script=${CMAKE_CURRENT_LIST_DIR}/exports.map"
  -Wl,--gc-sections
  -Wl,--exclude-libs,ALL
  -Wl,--icf=all
  -Wl,-z,max-page-size=16384
  -Wl,--no-undefined)
set_property(TARGET tinyrtc_jni APPEND PROPERTY LINK_DEPENDS "${CMAKE_CURRENT_LIST_DIR}/exports.map")
