# Phosphor's AirPlay shim: builds the vendored airplay2-sender-cpp library
# (Apache-2.0) and a small C API over it. Driven by build.rs (cmake crate)
# when the `airplay` cargo feature is on.
cmake_minimum_required(VERSION 3.16)
project(phosphor_airplay LANGUAGES C CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)

set(AIRPLAY_BUILD_EXAMPLE OFF CACHE BOOL "" FORCE)
set(AIRPLAY_BUILD_TESTS   OFF CACHE BOOL "" FORCE)
set(AIRPLAY_BUILD_QT_HOST OFF CACHE BOOL "" FORCE)
add_subdirectory(${CMAKE_CURRENT_SOURCE_DIR}/../../vendor/airplay2-sender-cpp airplay2)

add_library(phosphor_airplay STATIC phosphor_airplay.cpp)
target_link_libraries(phosphor_airplay PUBLIC raop_sender)
