/*
 * Copyright 2025 LiveKit, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#include "livekit/global_task_queue.h"

#include "api/task_queue/default_task_queue_factory.h"
#include "api/task_queue/task_queue_factory.h"

#if defined(WEBRTC_LINUX) || defined(WEBRTC_ANDROID)
#include <pthread.h>
#include <sched.h>
#endif

namespace livekit_ffi {

namespace {

// libwebrtc runs its task queues (video encoders, decoders, render queues)
// at real-time priority where the platform allows it: high-priority queues
// are TIME_CRITICAL on Windows, and on Linux every queue gets SCHED_FIFO
// when permitted (as root, or with an rtprio limit). When a call saturates
// the CPU, those threads starve libwebrtc's own worker thread, frames queue
// up for it without bound and memory grows by megabytes a second. Here
// every queue runs at normal priority, so each thread gets its share.
class FairTaskQueueFactory : public webrtc::TaskQueueFactory {
 public:
  FairTaskQueueFactory() : base_(webrtc::CreateDefaultTaskQueueFactory()) {}

  std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter>
  CreateTaskQueue(absl::string_view name, Priority priority) const override {
    auto queue = base_->CreateTaskQueue(
        name, priority == Priority::kLow ? priority : Priority::kNormal);
#if defined(WEBRTC_LINUX) || defined(WEBRTC_ANDROID)
    // Even kNormal is SCHED_FIFO on Linux. The queue's thread has started
    // when CreateTaskQueue returns, so this runs before anything else on it.
    queue->PostTask([] {
      sched_param param = {};
      pthread_setschedparam(pthread_self(), SCHED_OTHER, &param);
    });
#endif
    return queue;
  }

 private:
  std::unique_ptr<webrtc::TaskQueueFactory> base_;
};

}  // namespace

std::unique_ptr<webrtc::TaskQueueFactory> CreateFairTaskQueueFactory() {
  return std::make_unique<FairTaskQueueFactory>();
}

webrtc::TaskQueueFactory* GetGlobalTaskQueueFactory() {
  static std::unique_ptr<webrtc::TaskQueueFactory> global_task_queue_factory =
      CreateFairTaskQueueFactory();
  return global_task_queue_factory.get();
}

}  // namespace livekit_ffi
