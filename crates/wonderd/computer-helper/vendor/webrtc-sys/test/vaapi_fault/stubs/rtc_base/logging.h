#pragma once
#include <ostream>
#include <iostream>
struct NullLog { template <class T> NullLog& operator<<(const T&) { return *this; } };
#define LS_ERROR 0
#define LS_WARNING 0
#define LS_INFO 0
#define RTC_LOG(x) NullLog()
