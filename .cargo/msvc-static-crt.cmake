# C libraries built with CMake (zlib-ng) take the static C runtime too.
# CMake ignores the -MT that the cmake crate puts in the flags and uses this.
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
