.. SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
.. SPDX-License-Identifier: Apache-2.0

NVIDIA usd-caches
=================

**usd-caches** is the caching layer of Omniverse Content Services. It brings a local node cache, a shared content cache, and a shared derived-data cache together in one repository, reducing repeated content transfers and derived-data generation by reusing data close to the applications and compute that consume it.

In this documentation you will find how to choose caches for a workflow, getting started guides for each cache, and how the components fit with the rest of Omniverse Content Services.

* :doc:`Choosing caches <getting_started/choosing_caches>` - Decide which caches your workflow needs and where to run them.
* :doc:`Local cache (Hub) <getting_started/local_cache>` - Install on every workstation or compute node that runs Kit-based applications or Client Library tools.
* :doc:`Shared content cache (OVCC) <getting_started/shared_content_cache>` - Deploy with Helm when many clients read the same content from S3, Azure Blob Storage, GCS, or Nucleus.
* :doc:`Shared derived-data cache (OVDC) <getting_started/shared_derived_cache>` - Deploy with Helm when many workers generate the same derived data.

.. note::

   Each component keeps its own version, build workspace, and deployment requirements. There is no combined build or installer; validate each component against its own release documentation.

Features
--------

* Reuse source content and derived data on a single node with the local cache, so repeated loads are served from local disk.
* Serve source content to many clients from one shared content cache placed in front of S3, Azure Blob Storage, GCS, or Nucleus.
* Share generated derived data across workers through the shared derived-data cache's gRPC ``Get``, ``Put``, and ``Touch`` operations.
* Install prebuilt executables, containers, and Helm charts from NGC, or build each component from source.

Prebuilt Releases
-----------------

.. list-table::
   :header-rows: 1

   * - Component
     - NGC
     - Published documentation
   * - Local cache (Hub)
     - `Collection <https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/collections/hub-workstation-cache>`_ · `Installers <https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/resources/hub_workstation_cache>`_ · `Container <https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/containers/hub_workstation_cache>`_
     - `Hub Workstation Cache <https://docs.omniverse.nvidia.com/utilities/latest/cache/hub-workstation.html>`_
   * - Shared content cache (OVCC)
     - `Helm chart <https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/helm-charts/ovcontentcache>`_ · `Container <https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/containers/usd-content-cache>`_
     - `Omniverse Content Cache <https://docs.omniverse.nvidia.com/ovcaches/ovcontentcache/latest/index.html>`_
   * - Shared derived-data cache (OVDC)
     - `Helm chart <https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/helm-charts/ovderivedcache>`_ · `Container <https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/containers/ovderivedcache>`_
     - `Omniverse Derived Cache <https://docs.omniverse.nvidia.com/ovcaches/ovderivedcache/latest/index.html>`_

Support
-------

Report documentation issues, installation problems, and runtime issues through the NVIDIA Omniverse developer forum.

https://forums.developer.nvidia.com/c/omniverse/300

License
-------

The source is licensed under the `Apache License 2.0 <https://github.com/NVIDIA-Omniverse/usd-caches/blob/main/LICENSE>`_. Each component also carries its own LICENSE file, and third-party components remain under their own licenses.

.. toctree::
   :maxdepth: 2
   :caption: Getting Started

   getting_started/choosing_caches
   getting_started/local_cache
   getting_started/shared_content_cache
   getting_started/shared_derived_cache

.. toctree::
   :maxdepth: 2
   :caption: Overview

   overview/components
   overview/content_services

.. toctree::
   :maxdepth: 2
   :caption: AI Coding Agents

   agents

.. toctree::
   :caption: Links

   GitHub Repository <https://github.com/NVIDIA-Omniverse/usd-caches>

Indices and Tables
==================

* :ref:`genindex`
* :ref:`search`
